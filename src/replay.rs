//! Checks a container change against real hits before it is saved: each
//! hit runs through the current container and through the changed one, and
//! the two runs are compared tag by tag: which tags fire for which events,
//! and every request each tag would send, parameter by parameter.
//!
//! Nothing is sent: vendors are answered with a canned success. Both runs of
//! a hit see the same time and the same random numbers, so what differs is
//! what the change did. The hits come from the trace buffer and from hits
//! pinned as test cases. Their user data and cookie values are already
//! masked, the same way for both runs.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::container::Container;
use crate::request::IncomingRequest;
use crate::sandbox::{Engine, Outgoing, RunResult, SendResult, Transport};

/// Most hits replayed from the trace buffer.
pub const MAX_HITS: usize = 200;
/// Most groups of differences returned.
const MAX_GROUPS: usize = 50;
const BUDGET: Duration = Duration::from_secs(2);
const MEMORY: usize = 64 * 1024 * 1024;
/// Longest value shown in an example.
const SHOWN: usize = 120;

/// One hit to replay.
pub struct Hit {
    /// The trace id, for links; none for a pinned hit.
    pub trace: Option<u64>,
    /// The pinned hit's label.
    pub pinned: Option<String>,
    pub at: u64,
    pub request: IncomingRequest,
}

/// A hit's request as a trace or a pinned hit keeps it.
pub fn request(v: &Value) -> Option<IncomingRequest> {
    let text = |k: &str| v[k].as_str().unwrap_or_default().to_string();
    let headers = v["headers"]
        .as_array()?
        .iter()
        .filter_map(|h| Some((h[0].as_str()?.to_string(), h[1].as_str()?.to_string())))
        .collect();
    Some(IncomingRequest {
        method: v["method"].as_str()?.to_string(),
        path: v["path"].as_str()?.to_string(),
        query: text("query"),
        headers,
        body: text("body"),
        remote_address: text("remote_address"),
    })
}

/// Answers every send with a success and sends nothing, keeping what would
/// have gone out: the run's record has the URLs but not the bodies.
#[derive(Default)]
struct Canned {
    seen: std::sync::Mutex<Vec<Outgoing>>,
}

impl Canned {
    fn take(&self) -> Vec<Outgoing> {
        std::mem::take(&mut self.seen.lock().expect("canned"))
    }
}

impl Transport for Canned {
    fn send_all(&self, requests: Vec<Outgoing>) -> Vec<Result<SendResult, String>> {
        let n = requests.len();
        self.seen.lock().expect("canned").extend(requests);
        (0..n)
            .map(|_| {
                Ok(SendResult {
                    status: 200,
                    headers: vec![],
                    body: "{}".into(),
                })
            })
            .collect()
    }
}

/// Answers every send with a success and sends nothing: a dry preview.
pub struct Dry;

impl Transport for Dry {
    fn send_all(&self, requests: Vec<Outgoing>) -> Vec<Result<SendResult, String>> {
        requests
            .iter()
            .map(|_| {
                Ok(SendResult {
                    status: 200,
                    headers: vec![],
                    body: "{}".into(),
                })
            })
            .collect()
    }
}

fn engine(c: Arc<Container>) -> Result<(Engine, Arc<Canned>), String> {
    let canned = Arc::new(Canned::default());
    let e = Engine::new(c, Default::default(), canned.clone(), BUDGET, MEMORY)
        .map_err(|e| e.to_string())?;
    Ok((e, canned))
}

/// Runs one hit, with what each send carried.
fn run(e: &(Engine, Arc<Canned>), hit: &Hit) -> (RunResult, Vec<Outgoing>) {
    e.0.set_clock(Some(hit.at));
    let r = e.0.run(hit.request.clone());
    (r, e.1.take())
}

/// A send as its parameters: the query's, and the body's (JSON flattened to
/// dotted keys, or form pairs).
fn params(url: &str, body: &str) -> (String, BTreeMap<String, String>) {
    let (base, query) = url.split_once('?').unwrap_or((url, ""));
    let mut out = BTreeMap::new();
    for (k, v) in form_urlencoded::parse(query.as_bytes()) {
        out.insert(k.into_owned(), v.into_owned());
    }
    let body = body.trim();
    if let Some(v) = serde_json::from_str::<Value>(body)
        .ok()
        .filter(|v| v.is_object() || v.is_array())
    {
        flatten(&v, "body", &mut out);
    } else if !body.is_empty() {
        for (k, v) in form_urlencoded::parse(body.as_bytes()) {
            out.insert(format!("body.{k}"), v.into_owned());
        }
    }
    let endpoint = base.split_once("://").map_or(base, |(_, r)| r).to_string();
    (endpoint, out)
}

fn flatten(v: &Value, at: &str, out: &mut BTreeMap<String, String>) {
    match v {
        Value::Object(m) => m
            .iter()
            .for_each(|(k, x)| flatten(x, &format!("{at}.{k}"), out)),
        Value::Array(a) => a
            .iter()
            .enumerate()
            .for_each(|(i, x)| flatten(x, &format!("{at}.{i}"), out)),
        Value::String(s) => {
            out.insert(at.to_string(), s.clone());
        }
        other => {
            out.insert(at.to_string(), other.to_string());
        }
    }
}

/// A send's endpoint and parameters.
type Params = (String, BTreeMap<String, String>);

/// The sends of a run, by tag, in order. The record and the transport see
/// sends in the same order.
fn sends(r: &RunResult, outgoing: &[Outgoing]) -> BTreeMap<String, Vec<Params>> {
    let mut out: BTreeMap<String, Vec<_>> = BTreeMap::new();
    for (i, s) in r.sends.iter().enumerate() {
        let body = outgoing
            .get(i)
            .filter(|o| o.url == s.url)
            .map_or("", |o| o.body.as_str());
        out.entry(s.from.clone())
            .or_default()
            .push(params(&s.url, body));
    }
    out
}

/// Which tags ran for which events in a run.
fn fired(r: &RunResult) -> BTreeSet<(String, String)> {
    r.tags
        .iter()
        .map(|t| {
            let event = r
                .events
                .get(t.event)
                .and_then(|e| e["event_name"].as_str())
                .unwrap_or("(no name)");
            (t.tag.clone(), event.to_string())
        })
        .collect()
}

/// The secrets the two containers' tags hold: string settings whose names
/// look secret (as the admin API redacts them). Their values are hidden
/// wherever a send carried them, so a check never shows a token, and the
/// AI review it goes to never gets one.
struct Secrets(Vec<String>);

impl Secrets {
    fn of(containers: &[&Container]) -> Self {
        fn collect(v: &Value, secret: bool, out: &mut Vec<String>) {
            match v {
                Value::Object(m) => m
                    .iter()
                    .for_each(|(k, x)| collect(x, secret || crate::admin::secret_name(k), out)),
                Value::Array(a) => a.iter().for_each(|x| collect(x, secret, out)),
                // Very short values would hide ordinary text.
                Value::String(s) if secret && s.len() >= 6 => out.push(s.clone()),
                _ => {}
            }
        }
        let mut out = Vec::new();
        for c in containers {
            for t in &c.tags {
                collect(&t.data, false, &mut out);
            }
        }
        // Longest first, so a secret that contains another is hidden whole.
        out.sort_by_key(|s| std::cmp::Reverse(s.len()));
        out.dedup();
        Secrets(out)
    }

    fn hide(&self, s: &str) -> String {
        self.0.iter().fold(s.to_string(), |acc, x| {
            acc.replace(x.as_str(), "[redacted]")
        })
    }

    /// A parameter's value as a check shows it: hidden when its key looks
    /// secret, with known secrets hidden anywhere in it, and cut short.
    fn shown(&self, key: &str, v: Option<&String>) -> Value {
        v.map_or(Value::Null, |s| {
            let last = key.rsplit('.').next().unwrap_or(key);
            if crate::admin::secret_name(last) {
                return json!("[redacted]");
            }
            json!(self.hide(s).chars().take(SHOWN).collect::<String>())
        })
    }
}

#[derive(Default)]
struct Group {
    hits: BTreeSet<usize>,
    example: Option<Value>,
}

/// Runs `hits` through `old` and `new` and sums up what differs.
pub fn compare(old: Arc<Container>, new: Arc<Container>, hits: &[Hit]) -> Value {
    let secrets = Secrets::of(&[&old, &new]);
    let (a, b) = match (engine(old), engine(new)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => return json!({ "error": e }),
    };
    let mut groups: BTreeMap<(String, String, String, String), Group> = BTreeMap::new();
    let mut changed: BTreeSet<usize> = BTreeSet::new();
    let mut pinned_changed: Vec<Value> = Vec::new();
    for (i, hit) in hits.iter().enumerate() {
        let ((ra, oa), (rb, ob)) = (run(&a, hit), run(&b, hit));
        let at_hit = |extra: Value| {
            let mut e = json!({ "trace": hit.trace, "pinned": hit.pinned });
            if let (Some(m), Value::Object(x)) = (e.as_object_mut(), extra) {
                m.extend(x);
            }
            e
        };
        let mut add = |kind: &str, tag: &str, event: &str, key: &str, extra: Value| {
            let g = groups
                .entry((
                    tag.to_string(),
                    kind.to_string(),
                    event.to_string(),
                    key.to_string(),
                ))
                .or_default();
            g.hits.insert(i);
            if g.example.is_none() {
                g.example = Some(at_hit(extra));
            }
            changed.insert(i);
        };
        let (fa, fb) = (fired(&ra), fired(&rb));
        for (tag, event) in fa.difference(&fb) {
            add("stops", tag, event, "", json!({}));
        }
        for (tag, event) in fb.difference(&fa) {
            add("starts", tag, event, "", json!({}));
        }
        let (sa, sb) = (sends(&ra, &oa), sends(&rb, &ob));
        let tags: BTreeSet<&String> = sa.keys().chain(sb.keys()).collect();
        for tag in tags {
            let empty = Vec::new();
            let (xa, xb) = (sa.get(tag).unwrap_or(&empty), sb.get(tag).unwrap_or(&empty));
            if xa.len() != xb.len() && !xa.is_empty() && !xb.is_empty() {
                add(
                    "send_count",
                    tag,
                    "",
                    "",
                    json!({ "before": xa.len(), "after": xb.len() }),
                );
            }
            for ((ea, pa), (eb, pb)) in xa.iter().zip(xb.iter()) {
                if ea != eb {
                    add(
                        "endpoint",
                        tag,
                        "",
                        "",
                        json!({ "before": secrets.hide(ea), "after": secrets.hide(eb) }),
                    );
                }
                let keys: BTreeSet<&String> = pa.keys().chain(pb.keys()).collect();
                for k in keys {
                    let (va, vb) = (pa.get(k), pb.get(k));
                    if va == vb {
                        continue;
                    }
                    let kind = match (va, vb) {
                        (Some(_), None) => "param_removed",
                        (None, Some(_)) => "param_added",
                        _ => "param_changed",
                    };
                    add(
                        kind,
                        tag,
                        "",
                        k,
                        json!({ "before": secrets.shown(k, va), "after": secrets.shown(k, vb) }),
                    );
                }
            }
        }
        if changed.contains(&i)
            && let Some(label) = &hit.pinned
        {
            pinned_changed.push(json!(label));
        }
    }
    let mut list: Vec<(usize, Value)> = groups
        .into_iter()
        .map(|((tag, kind, event, key), g)| {
            (g.hits.len(), json!({ "tag": tag, "kind": kind, "event": event, "key": key, "hits": g.hits.len(), "example": g.example }))
        })
        .collect();
    list.sort_by_key(|x| std::cmp::Reverse(x.0));
    let total = list.len();
    let list: Vec<Value> = list.into_iter().take(MAX_GROUPS).map(|(_, v)| v).collect();
    let pinned = hits.iter().filter(|h| h.pinned.is_some()).count();
    json!({
        "hits": hits.len(),
        "changed_hits": changed.len(),
        "pinned": pinned,
        "pinned_changed": pinned_changed,
        "groups": list,
        "more_groups": total.saturating_sub(MAX_GROUPS),
    })
}

/// Keeps what of a request a pinned hit needs.
pub fn pinned_entry(
    id: &str,
    label: &str,
    tenant: Option<&str>,
    at: u64,
    request: &Value,
) -> Value {
    let mut m = Map::new();
    for k in [
        "method",
        "path",
        "query",
        "headers",
        "body",
        "remote_address",
    ] {
        m.insert(k.into(), request[k].clone());
    }
    json!({ "id": id, "label": label, "tenant": tenant, "at": at, "request": m })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(q: &str, trace: u64) -> Hit {
        // Consent granted, so the ad tags send.
        let q = format!("{q}&gcd=13r3r3r2r5l1");
        Hit {
            trace: Some(trace),
            pinned: None,
            at: 1_790_000_000_000,
            request: IncomingRequest {
                method: "GET".into(),
                path: "/g/collect".into(),
                query: q,
                headers: vec![
                    ("user-agent".into(), "Mozilla/5.0 Test".into()),
                    ("host".into(), "example.com".into()),
                ],
                remote_address: "203.0.113.9".into(),
                ..Default::default()
            },
        }
    }

    fn with_meta(fire_when: Value, rules: Value) -> Arc<Container> {
        let mut c = Container::builtin();
        c.tags.push(
            serde_json::from_value(json!({
                "name": "Meta", "template": "meta_capi", "fire_when": fire_when,
                "data": { "pixel_id": "1", "access_token": "t" }
            }))
            .unwrap(),
        );
        c.transformations = serde_json::from_value(rules).unwrap();
        c.compile().unwrap();
        Arc::new(c)
    }

    #[test]
    fn same_container_same_result() {
        let c = with_meta(json!([]), json!([]));
        let hits = vec![hit(
            "v=2&tid=G-1&cid=1.2&en=purchase&epn.value=5&cu=EUR&ep.event_id=e1",
            1,
        )];
        let r = compare(c.clone(), c, &hits);
        assert_eq!(r["changed_hits"], 0, "{r}");
        assert_eq!(r["hits"], 1);
    }

    #[test]
    fn a_change_shows_tag_by_tag() {
        let old = with_meta(json!([]), json!([]));
        let new = with_meta(
            json!([{ "key": "event_name", "op": "equals", "value": "purchase" }]),
            json!([{ "name": "EUR", "steps": [{ "op": "set", "key": "currency", "value": "EUR" }] }]),
        );
        let hits = vec![
            hit(
                "v=2&tid=G-1&cid=1.2&en=purchase&epn.value=5&ep.event_id=e1",
                1,
            ),
            hit("v=2&tid=G-1&cid=1.2&en=page_view", 2),
        ];
        let r = compare(old, new, &hits);
        assert_eq!(r["changed_hits"], 2, "{r}");
        let groups = r["groups"].as_array().unwrap();
        let find = |kind: &str, tag: &str| {
            groups
                .iter()
                .find(|g| g["kind"] == kind && g["tag"] == tag)
                .cloned()
        };
        let stops = find("stops", "Meta").expect("Meta stops for page_view");
        assert_eq!(stops["event"], "page_view");
        assert_eq!(stops["example"]["trace"], 2);
        let cu = find("param_added", "GA4").expect("GA4 gets cu");
        assert_eq!(cu["key"], "cu");
        assert_eq!(cu["example"]["after"], "EUR");
        assert!(
            groups.iter().any(|g| g["tag"] == "Meta"
                && g["kind"] == "param_added"
                && g["key"].as_str().unwrap().contains("currency")),
            "{r}"
        );
    }

    #[test]
    fn a_changed_secret_never_shows() {
        let with_token = |token: &str, extra: &str| {
            let mut c = Container::builtin();
            c.tags.push(
                serde_json::from_value(json!({
                    "name": "Meta", "template": "meta_capi",
                    "data": { "pixel_id": "1", "access_token": token, "test_event_code": extra }
                }))
                .unwrap(),
            );
            c.compile().unwrap();
            Arc::new(c)
        };
        let (old_token, new_token) = ("EAAold-secret-token-1234", "EAAnew-secret-token-5678");
        let hits = vec![hit(
            "v=2&tid=G-1&cid=1.2&en=purchase&epn.value=5&cu=EUR&ep.event_id=e1",
            1,
        )];
        // The token's own key, and the token copied under a harmless one.
        let r = compare(
            with_token(old_token, old_token),
            with_token(new_token, new_token),
            &hits,
        );
        let text = r.to_string();
        assert_eq!(r["changed_hits"], 1, "{r}");
        assert!(
            text.contains("access_token"),
            "the change is still listed: {r}"
        );
        for t in [old_token, new_token] {
            assert!(!text.contains(t), "{t} leaked: {r}");
        }
        assert!(text.contains("[redacted]"), "{r}");
    }

    #[test]
    fn requests_come_back_from_a_trace() {
        let v = json!({ "method": "POST", "path": "/g/collect", "query": "v=2", "headers": [["user-agent", "UA"]], "body": "en=x", "remote_address": "1.2.3.4" });
        let r = request(&v).unwrap();
        assert_eq!(
            r.headers,
            vec![("user-agent".to_string(), "UA".to_string())]
        );
        assert_eq!(r.body, "en=x");
        assert!(request(&json!({})).is_none());
    }
}
