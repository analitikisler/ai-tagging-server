//! Native bindings handed to the prelude for one template context. The
//! prelude wraps them into the sGTM APIs; templates never see them directly.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use hmac::{Hmac, KeyInit, Mac};
use rquickjs::{Ctx, Function, Object};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{EventRun, Outgoing, State, Storage};
use crate::{mpv1, mpv2};

/// Most keys one `templateDataStorage` map holds.
const STORAGE_KEYS: usize = 10_000;

const DEFAULT_SEND_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) fn bindings<'js>(
    ctx: &Ctx<'js>,
    state: Rc<RefCell<State>>,
    storage: Storage,
    idx: usize,
) -> rquickjs::Result<Object<'js>> {
    let o = Object::new(ctx.clone())?;

    let st = state.clone();
    o.set(
        "request",
        Function::new(ctx.clone(), move || st.borrow().req_json.clone())?,
    )?;

    let st = state.clone();
    o.set(
        "event",
        Function::new(ctx.clone(), move || {
            // A tag reads the event as its transformations left it.
            let s = st.borrow();
            let m = &s.meta[idx];
            m.event
                .and_then(|e| s.events.get(e))
                .map(|e| e.views.get(&m.name).unwrap_or(&e.data).to_string())
                .unwrap_or_else(|| "{}".into())
        })?,
    )?;

    let st = state.clone();
    o.set(
        "claim",
        Function::new(ctx.clone(), move || {
            let mut s = st.borrow_mut();
            if s.claimed.is_none() && !s.meta[idx].is_tag {
                s.claimed = Some(idx);
            }
        })?,
    )?;

    let st = state.clone();
    o.set(
        "setStatus",
        Function::new(ctx.clone(), move |code: i32| {
            if (100..=599).contains(&code) {
                st.borrow_mut().response.status = code as u16;
            }
        })?,
    )?;

    let st = state.clone();
    o.set(
        "setHeader",
        Function::new(
            ctx.clone(),
            move |name: String, value: String, append: bool| {
                let mut s = st.borrow_mut();
                let headers = &mut s.response.headers;
                if !append {
                    headers.retain(|(k, _)| *k != name);
                }
                if headers.len() < 100 {
                    headers.push((name, value));
                }
            },
        )?,
    )?;

    let st = state.clone();
    o.set(
        "setBody",
        Function::new(ctx.clone(), move |body: String, is_base64: bool| {
            let bytes = if is_base64 {
                base64::engine::general_purpose::STANDARD
                    .decode(body.as_bytes())
                    .unwrap_or_default()
            } else {
                body.into_bytes()
            };
            st.borrow_mut().response.body = bytes;
        })?,
    )?;

    let st = state.clone();
    o.set(
        "returnResponse",
        Function::new(ctx.clone(), move || {
            let mut s = st.borrow_mut();
            if s.claimed == Some(idx) {
                s.returned = true;
            }
        })?,
    )?;

    let st = state.clone();
    o.set(
        "runContainer",
        Function::new(ctx.clone(), move |json: String| -> i32 {
            let mut s = st.borrow_mut();
            if s.events.len() >= 100 {
                return -1;
            }
            let data: Value =
                serde_json::from_str(&json).unwrap_or(Value::Object(Default::default()));
            s.events.push(EventRun {
                data,
                views: Default::default(),
                client_ctx: idx,
                started: false,
                tag_ctxs: vec![],
                notified: false,
            });
            (s.events.len() - 1) as i32
        })?,
    )?;

    let st = state.clone();
    o.set(
        "log",
        Function::new(ctx.clone(), move |line: String| {
            let mut s = st.borrow_mut();
            if s.logs.len() < 1000 {
                let mut line = format!("[{}] {line}", s.meta[idx].name);
                line.truncate(1024);
                s.logs.push(line);
            }
        })?,
    )?;

    let st = state.clone();
    o.set(
        "send",
        Function::new(
            ctx.clone(),
            move |url: String, opts: String, body: String| -> i32 {
                let opts: Value = serde_json::from_str(&opts).unwrap_or_default();
                let headers = opts["headers"]
                    .as_object()
                    .map(|h| {
                        h.iter()
                            .map(|(k, v)| {
                                (
                                    k.clone(),
                                    v.as_str()
                                        .map(String::from)
                                        .unwrap_or_else(|| v.to_string()),
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let mut s = st.borrow_mut();
                let remaining = s.deadline.saturating_duration_since(Instant::now());
                let timeout = opts["timeout"]
                    .as_u64()
                    .map(Duration::from_millis)
                    .unwrap_or(DEFAULT_SEND_TIMEOUT)
                    .min(remaining);
                if s.sends.len() >= 1000 {
                    return -1;
                }
                let id = s.next_send;
                s.next_send += 1;
                s.sends.push((
                    id,
                    idx,
                    Outgoing {
                        url,
                        method: opts["method"]
                            .as_str()
                            .unwrap_or("GET")
                            .to_ascii_uppercase(),
                        headers,
                        body,
                        timeout,
                        google_auth: false,
                    },
                ));
                id as i32
            },
        )?,
    )?;

    // BigQuery.insert: a streaming insert into one table, sent with the
    // server's Google token. Returns the send's id, or a negative number:
    // -1 too many sends, -2 a bad project, dataset or table id, -3 rows that
    // aren't a list of 1 to 500 objects.
    let st = state.clone();
    o.set(
        "bigqueryInsert",
        Function::new(
            ctx.clone(),
            move |project: String,
                  dataset: String,
                  table: String,
                  rows: String,
                  opts: String|
                  -> i32 {
                let Some(url) = bigquery_insert_url(&project, &dataset, &table) else {
                    return -2;
                };
                let Some(body) = bigquery_insert_body(&rows, &opts) else {
                    return -3;
                };
                let mut s = st.borrow_mut();
                if s.sends.len() >= 1000 {
                    return -1;
                }
                let timeout =
                    DEFAULT_SEND_TIMEOUT.min(s.deadline.saturating_duration_since(Instant::now()));
                let id = s.next_send;
                s.next_send += 1;
                s.sends.push((
                    id,
                    idx,
                    Outgoing {
                        url,
                        method: "POST".into(),
                        headers: vec![("content-type".into(), "application/json".into())],
                        body,
                        timeout,
                        google_auth: true,
                    },
                ));
                id as i32
            },
        )?,
    )?;

    let st = state.clone();
    o.set(
        "isMpv2",
        Function::new(ctx.clone(), move || mpv2::is_mpv2(&st.borrow().req))?,
    )?;

    let st = state.clone();
    o.set(
        "extractMpv2",
        Function::new(ctx.clone(), move || {
            Value::Array(mpv2::extract_events(&st.borrow().req)).to_string()
        })?,
    )?;

    let st = state.clone();
    o.set(
        "isMpv1",
        Function::new(ctx.clone(), move || mpv1::is_mpv1(&st.borrow().req))?,
    )?;

    let st = state.clone();
    o.set(
        "extractMpv1",
        Function::new(ctx.clone(), move || {
            Value::Array(mpv1::extract_events(&st.borrow().req)).to_string()
        })?,
    )?;

    // addEventCallback: only tags have an event to wait for.
    let st = state.clone();
    o.set(
        "addEventCallback",
        Function::new(ctx.clone(), move || -> i32 {
            let mut s = st.borrow_mut();
            let Some(ev) = s.meta[idx].event else {
                return -1;
            };
            if s.callbacks.len() >= 1000 {
                return -1;
            }
            let id = s.callbacks.len() as u32;
            s.callbacks.push((ev, idx, id));
            id as i32
        })?,
    )?;

    let st = state.clone();
    o.set(
        "addListener",
        Function::new(ctx.clone(), move |kind: String| -> i32 {
            let mut s = st.borrow_mut();
            if s.listeners.len() >= 1000 {
                return -1;
            }
            let id = s.listeners.len() as u32;
            s.listeners.push((kind, idx, id));
            id as i32
        })?,
    )?;

    let st = state.clone();
    o.set(
        "hasListener",
        Function::new(ctx.clone(), move |kind: String| {
            st.borrow().listeners.iter().any(|(t, _, _)| *t == kind)
        })?,
    )?;

    let st = state.clone();
    o.set(
        "sendMessage",
        Function::new(ctx.clone(), move |kind: String, json: String| {
            let mut s = st.borrow_mut();
            if s.messages.len() < 1000 {
                s.messages.push((kind, json));
            }
        })?,
    )?;

    let st = state.clone();
    o.set(
        "tagDone",
        Function::new(ctx.clone(), move |ok: bool| {
            let mut s = st.borrow_mut();
            if s.meta[idx].is_tag {
                s.tag_done.entry(idx).or_insert(ok);
            }
        })?,
    )?;

    // A tag that decided not to send: it counts as finished (addEventCallback
    // sees "success", as in sGTM) but is reported as skipped, with the reason.
    let st = state.clone();
    o.set(
        "tagSkipped",
        Function::new(ctx.clone(), move |reason: String| {
            let mut s = st.borrow_mut();
            if s.meta[idx].is_tag && !s.tag_done.contains_key(&idx) {
                s.tag_done.insert(idx, true);
                s.tag_skipped.insert(idx, reason);
            }
        })?,
    )?;

    let st = state.clone();
    o.set(
        "clientName",
        Function::new(ctx.clone(), move || {
            let s = st.borrow();
            s.claimed
                .map(|c| s.meta[c].name.clone())
                .unwrap_or_default()
        })?,
    )?;

    let st = state.clone();
    o.set(
        "containerVersion",
        Function::new(ctx.clone(), move || {
            serde_json::json!({
                "containerId": "",
                "version": st.borrow().container_version,
                "environmentName": "",
                "environmentMode": false,
                "debugMode": false,
                "previewMode": false,
            })
            .to_string()
        })?,
    )?;

    o.set(
        "etld1",
        Function::new(ctx.clone(), |host: String| {
            let host = host.trim_end_matches('.').to_ascii_lowercase();
            psl::domain_str(&host).map(String::from).unwrap_or_default()
        })?,
    )?;

    o.set(
        "toBase64",
        Function::new(ctx.clone(), |s: String| {
            base64::engine::general_purpose::STANDARD.encode(s.as_bytes())
        })?,
    )?;
    o.set(
        "fromBase64",
        Function::new(ctx.clone(), |s: String| {
            base64::engine::general_purpose::STANDARD
                .decode(s.trim().as_bytes())
                .ok()
                .and_then(|b| String::from_utf8(b).ok())
                .unwrap_or_else(|| "\u{0}invalid".into())
        })?,
    )?;
    o.set(
        "sha256",
        Function::new(ctx.clone(), |input: String, encoding: String| {
            let digest = Sha256::digest(input.as_bytes());
            if encoding == "hex" {
                digest
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            } else {
                base64::engine::general_purpose::STANDARD.encode(digest)
            }
        })?,
    )?;

    o.set(
        "hmacSha256",
        Function::new(
            ctx.clone(),
            |input: String, key: String, encoding: String| {
                // HMAC accepts keys of any length, so this can't fail.
                let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key.as_bytes())
                    .expect("hmac takes any key length");
                mac.update(input.as_bytes());
                let digest = mac.finalize().into_bytes();
                if encoding == "hex" {
                    digest
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>()
                } else {
                    base64::engine::general_purpose::STANDARD.encode(digest)
                }
            },
        )?,
    )?;

    // templateDataStorage: one map per template (or per group of templates
    // that share one, see `Container::storage_scope`), shared across runs.
    let template_key = state.borrow().meta[idx].storage.clone();
    let (s, t) = (storage.clone(), template_key.clone());
    o.set(
        "storageGet",
        Function::new(ctx.clone(), move |key: String| {
            s.lock()
                .ok()
                .and_then(|m| m.get(&t).and_then(|m| m.get(&key).cloned()))
                .unwrap_or_default()
        })?,
    )?;
    let (s, t) = (storage.clone(), template_key.clone());
    o.set(
        "storageSet",
        Function::new(ctx.clone(), move |key: String, value: String| {
            if let Ok(mut m) = s.lock() {
                let m = m.entry(t.clone()).or_default();
                // At the cap, a new key pushes out an existing one, so a
                // cache keeps working once it is full.
                if m.len() >= STORAGE_KEYS
                    && !m.contains_key(&key)
                    && let Some(old) = m.keys().next().cloned()
                {
                    m.remove(&old);
                }
                m.insert(key, value);
            }
        })?,
    )?;
    let (s, t) = (storage.clone(), template_key.clone());
    o.set(
        "storageRemove",
        Function::new(ctx.clone(), move |key: String| {
            if let Ok(mut m) = s.lock()
                && let Some(m) = m.get_mut(&t)
            {
                m.remove(&key);
            }
        })?,
    )?;
    let (s, t) = (storage, template_key);
    o.set(
        "storageClear",
        Function::new(ctx.clone(), move || {
            if let Ok(mut m) = s.lock() {
                m.remove(&t);
            }
        })?,
    )?;

    Ok(o)
}

/// Where a streaming insert into `project.dataset.table` goes, or None when an
/// id has characters BigQuery doesn't allow. Project ids may carry a domain
/// (`example.com:project`); a table id may name a partition (`events$20261015`).
pub(crate) fn bigquery_insert_url(project: &str, dataset: &str, table: &str) -> Option<String> {
    let ok = |s: &str, max: usize, extra: &[char]| {
        !s.is_empty()
            && s.len() <= max
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || extra.contains(&c))
    };
    if !ok(project, 100, &['-', '.', ':'])
        || !ok(dataset, 1024, &[])
        || !ok(table, 1024, &['-', '$'])
    {
        return None;
    }
    let table = table.replace('$', "%24");
    Some(format!(
        "https://bigquery.googleapis.com/bigquery/v2/projects/{project}/datasets/{dataset}/tables/{table}/insertAll"
    ))
}

/// The insertAll body for a JSON list of row objects. Each row's insertId is
/// a hash of the row, so BigQuery drops a copy that a retried send delivers
/// twice.
pub(crate) fn bigquery_insert_body(rows: &str, opts: &str) -> Option<String> {
    let rows: Vec<Value> = serde_json::from_str(rows).ok()?;
    if rows.is_empty() || rows.len() > 500 || !rows.iter().all(Value::is_object) {
        return None;
    }
    let opts: Value = serde_json::from_str(opts).unwrap_or_default();
    let rows: Vec<Value> = rows
        .into_iter()
        .map(|row| {
            let digest = Sha256::digest(row.to_string().as_bytes());
            let id: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
            serde_json::json!({ "insertId": id, "json": row })
        })
        .collect();
    Some(
        serde_json::json!({
            "kind": "bigquery#tableDataInsertAllRequest",
            "skipInvalidRows": opts["skipInvalidRows"].as_bool().unwrap_or(false),
            "ignoreUnknownValues": opts["ignoreUnknownValues"].as_bool().unwrap_or(false),
            "rows": rows,
        })
        .to_string(),
    )
}
