//! Universal Analytics Measurement Protocol v1 (`/collect`), behind the
//! `isRequestMpv1` and `extractEventsFromMpv1` sandbox APIs.
//!
//! One hit is one event. Parameters come from the query string, and from the
//! form-encoded body of a `POST` (body values win). Hit types map to GA4-style
//! event names: `pageview` to `page_view`, `screenview` to `screen_view`, and
//! an `event` hit to its action (`ea`), or `event` when it has none. Other hit
//! types keep their own name. Custom dimensions and metrics (`cd1`, `cm1`) are
//! kept under their own names.

use serde_json::{Map, Number, Value};

use crate::mpv2::RAW_HIT_KEY;
use crate::request::IncomingRequest;

const COLLECT_PATHS: [&str; 3] = ["/collect", "/r/collect", "/j/collect"];

pub fn is_mpv1(req: &IncomingRequest) -> bool {
    COLLECT_PATHS.contains(&req.path.as_str())
        && params(req).iter().any(|(k, v)| k == "v" && v == "1")
}

pub fn extract_events(req: &IncomingRequest) -> Vec<Value> {
    vec![to_event(&params(req), req)]
}

fn params(req: &IncomingRequest) -> Vec<(String, String)> {
    let parse = |s: &str| -> Vec<(String, String)> {
        form_urlencoded::parse(s.trim_start_matches('?').trim().as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    };
    let body = parse(&req.body);
    let mut out: Vec<(String, String)> = parse(&req.query)
        .into_iter()
        .filter(|(k, _)| !body.iter().any(|(bk, _)| bk == k))
        .collect();
    out.extend(body);
    out
}

fn number(v: &str) -> Option<Value> {
    if let Ok(i) = v.parse::<i64>() {
        return Some(Value::Number(i.into()));
    }
    v.parse::<f64>()
        .ok()
        .and_then(Number::from_f64)
        .map(Value::Number)
}

fn to_event(params: &[(String, String)], req: &IncomingRequest) -> Value {
    let get = |key: &str| {
        params
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.is_empty())
    };
    let str_v = |v: &str| Value::String(v.to_string());
    let mut ev = Map::new();

    let name = match get("t").unwrap_or("pageview") {
        "pageview" => "page_view".to_string(),
        "screenview" => "screen_view".to_string(),
        "event" => get("ea").unwrap_or("event").to_string(),
        other => other.to_string(),
    };
    ev.insert("event_name".into(), Value::String(name));

    for (k, v) in params {
        let key = k.as_str();
        match key {
            "cid" => ev.insert("client_id".into(), str_v(v)),
            "uid" => ev.insert("user_id".into(), str_v(v)),
            "dl" => ev.insert("page_location".into(), str_v(v)),
            "dr" => ev.insert("page_referrer".into(), str_v(v)),
            "dt" => ev.insert("page_title".into(), str_v(v)),
            "dp" => ev.insert("page_path".into(), str_v(v)),
            "dh" => ev.insert("page_hostname".into(), str_v(v)),
            "ul" => ev.insert("language".into(), str_v(v)),
            "sr" => ev.insert("screen_resolution".into(), str_v(v)),
            "cu" => ev.insert("currency".into(), str_v(v)),
            "cd" => ev.insert("screen_name".into(), str_v(v)),
            "ec" => ev.insert("event_category".into(), str_v(v)),
            "ea" => ev.insert("event_action".into(), str_v(v)),
            "el" => ev.insert("event_label".into(), str_v(v)),
            "ev" => ev.insert("value".into(), number(v).unwrap_or_else(|| str_v(v))),
            "ti" => ev.insert("transaction_id".into(), str_v(v)),
            "tr" => ev.insert("value".into(), number(v).unwrap_or_else(|| str_v(v))),
            "tid" => ev.insert("x-ga-measurement_id".into(), str_v(v)),
            "v" => ev.insert("x-ga-protocol_version".into(), str_v(v)),
            _ => {
                let is_custom = |p: &str| {
                    key.strip_prefix(p)
                        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                };
                if is_custom("cd") {
                    ev.insert(key.into(), str_v(v));
                } else if is_custom("cm") {
                    ev.insert(key.into(), number(v).unwrap_or_else(|| str_v(v)));
                }
                None
            }
        };
    }

    // Always the address the request came from; `uip` is ignored.
    ev.insert("ip_override".into(), str_v(&req.remote_address));
    if let Some(ua) = req.header("user-agent") {
        ev.insert("user_agent".into(), str_v(ua));
    }
    let raw = form_urlencoded::Serializer::new(String::new())
        .extend_pairs(params.iter())
        .finish();
    ev.insert(RAW_HIT_KEY.into(), Value::String(raw));
    Value::Object(ev)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(path: &str, query: &str, body: &str) -> IncomingRequest {
        IncomingRequest {
            method: if body.is_empty() { "GET" } else { "POST" }.into(),
            path: path.into(),
            query: query.into(),
            headers: vec![("user-agent".into(), "Test UA".into())],
            body: body.into(),
            remote_address: "203.0.113.7".into(),
        }
    }

    #[test]
    fn detects_mpv1() {
        assert!(is_mpv1(&req("/collect", "v=1&tid=UA-1-1&t=pageview", "")));
        assert!(is_mpv1(&req("/collect", "", "v=1&t=pageview")));
        assert!(!is_mpv1(&req("/collect", "v=2", "")));
        assert!(!is_mpv1(&req("/g/collect", "v=1", "")));
    }

    #[test]
    fn maps_a_page_view() {
        let ev = &extract_events(&req(
            "/collect",
            "v=1&tid=UA-1-1&cid=555&t=pageview&dl=https%3A%2F%2Fexample.com%2Fa&dt=Home&cd2=blue&cm1=4.5&uip=198.51.100.1",
            "",
        ))[0];
        assert_eq!(ev["event_name"], "page_view");
        assert_eq!(ev["client_id"], "555");
        assert_eq!(ev["page_location"], "https://example.com/a");
        assert_eq!(ev["page_title"], "Home");
        assert_eq!(ev["cd2"], "blue");
        assert_eq!(ev["cm1"], 4.5);
        assert_eq!(ev["x-ga-measurement_id"], "UA-1-1");
        assert_eq!(ev["ip_override"], "203.0.113.7");
        assert_eq!(ev["user_agent"], "Test UA");
        assert!(ev[RAW_HIT_KEY].as_str().unwrap().contains("t=pageview"));
    }

    #[test]
    fn event_hits_are_named_after_their_action_and_body_wins() {
        let ev = &extract_events(&req(
            "/collect",
            "v=1&t=pageview&cid=1",
            "t=event&ec=video&ea=play&el=intro&ev=3",
        ))[0];
        assert_eq!(ev["event_name"], "play");
        assert_eq!(ev["event_category"], "video");
        assert_eq!(ev["event_label"], "intro");
        assert_eq!(ev["value"], 3);
        assert_eq!(ev["client_id"], "1");
    }
}
