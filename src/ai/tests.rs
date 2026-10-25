use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use serde_json::{Value, json};

use super::*;

/// A stand-in for the Anthropic API: answers from a queue and keeps every
/// request it got.
#[derive(Clone, Default)]
struct Fake {
    answers: Arc<Mutex<VecDeque<(u16, Value)>>>,
    seen: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
}

impl Fake {
    fn answer(&self, status: u16, body: Value) {
        self.answers.lock().unwrap().push_back((status, body));
    }

    fn seen(&self) -> Vec<(HeaderMap, Value)> {
        self.seen.lock().unwrap().clone()
    }

    async fn start(&self) -> String {
        async fn messages(
            State(f): State<Fake>,
            headers: HeaderMap,
            Json(body): Json<Value>,
        ) -> impl IntoResponse {
            f.seen.lock().unwrap().push((headers, body));
            let (status, body) = f
                .answers
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or((500, json!({ "error": { "message": "no answer queued" } })));
            (
                StatusCode::from_u16(status).unwrap(),
                [("retry-after", "0")],
                Json(body),
            )
        }
        let app = Router::new()
            .route("/v1/messages", post(messages))
            .with_state(self.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        base
    }
}

/// A finished answer whose text is `answer` as JSON.
fn reply(answer: Value) -> Value {
    json!({
        "type": "message",
        "role": "assistant",
        "stop_reason": "end_turn",
        "content": [
            { "type": "thinking", "thinking": "", "signature": "x" },
            { "type": "text", "text": answer.to_string() },
        ],
        "usage": { "input_tokens": 1000, "output_tokens": 500, "cache_read_input_tokens": 2000, "cache_creation_input_tokens": 0 },
    })
}

fn ai(base: &str, model: &str, limit: u32) -> Ai {
    Ai::new("sk-test".into(), model.into(), base.into(), limit)
}

fn container() -> Value {
    json!({
        "tags": [
            { "name": "GA4", "template": "ga4_tag", "data": {}, "fire_when": [], "enabled": true },
            { "name": "Meta", "template": "meta_capi", "data": { "pixel_id": "1", "access_token": "[redacted]" }, "fire_when": [], "enabled": true },
            { "name": "Old GA4", "template": "ga4_tag", "data": {}, "fire_when": [], "enabled": true },
            { "name": "Unused", "template": "tiktok_events", "data": {}, "fire_when": [{ "key": "event_name", "op": "equals", "value": "Purchase" }], "enabled": true },
        ]
    })
}

fn simple_schema() -> Value {
    json!({ "type": "object", "additionalProperties": false, "required": ["ok"], "properties": { "ok": { "type": "boolean" } } })
}

#[tokio::test]
async fn asks_with_the_schema_effort_and_fallback() {
    let fake = Fake::default();
    let base = fake.start().await;
    let ai = ai(&base, "claude-opus-5-5", 10);
    fake.answer(200, reply(json!({ "ok": true })));
    let input = json!({ "hello": "world" });
    let answer = ai
        .ask(Ask {
            system: "be brief",
            schema: simple_schema(),
            effort: "medium",
            input: &input,
        })
        .await
        .unwrap();
    assert_eq!(answer, json!({ "ok": true }));

    let (headers, body) = fake.seen().remove(0);
    assert_eq!(headers["x-api-key"], "sk-test");
    assert_eq!(headers["anthropic-version"], API_VERSION);
    assert_eq!(headers["anthropic-beta"], FALLBACK_BETA);
    assert_eq!(body["model"], "claude-opus-5-5");
    assert_eq!(body["fallbacks"], "default");
    assert_eq!(body["output_config"]["effort"], "medium");
    assert_eq!(body["output_config"]["format"]["type"], "json_schema");
    assert_eq!(body["output_config"]["format"]["schema"], simple_schema());
    assert_eq!(body["system"][0]["text"], "be brief");
    assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(body["messages"][0]["content"], input.to_string());
    assert!(body.get("thinking").is_none() && body.get("temperature").is_none());

    let s = ai.status();
    assert_eq!(s["calls_today"], 1);
    assert_eq!(s["input_tokens"], 1000);
    assert_eq!(s["cache_read_tokens"], 2000);
    // 1000 × $4 + 2000 × $0.20 + 500 × $20, per million.
    assert_eq!(s["est_cost_usd"], 0.0144);
}

#[tokio::test]
async fn models_without_fallbacks_get_neither_the_field_nor_the_header() {
    let fake = Fake::default();
    let base = fake.start().await;
    let ai = ai(&base, "claude-haiku-5-5", 10);
    fake.answer(200, reply(json!({ "ok": true })));
    let input = json!({});
    ai.ask(Ask {
        system: "s",
        schema: simple_schema(),
        effort: "low",
        input: &input,
    })
    .await
    .unwrap();
    let (headers, body) = fake.seen().remove(0);
    assert!(headers.get("anthropic-beta").is_none());
    assert!(body.get("fallbacks").is_none());
}

#[tokio::test]
async fn refusals_cut_off_answers_and_bad_keys_are_errors() {
    let fake = Fake::default();
    let base = fake.start().await;
    let ai = ai(&base, "claude-opus-5-5", 10);
    let input = json!({});
    let ask = || Ask {
        system: "s",
        schema: simple_schema(),
        effort: "low",
        input: &input,
    };

    fake.answer(200, json!({ "stop_reason": "refusal", "stop_details": { "type": "refusal", "category": "cyber" }, "content": [] }));
    assert_eq!(ai.ask(ask()).await, Err(AiError::Refused("cyber".into())));

    fake.answer(
        200,
        json!({ "stop_reason": "max_tokens", "content": [{ "type": "text", "text": "{\"ok\":" }] }),
    );
    assert_eq!(ai.ask(ask()).await, Err(AiError::Truncated));

    fake.answer(401, json!({ "error": { "message": "invalid x-api-key" } }));
    assert_eq!(ai.ask(ask()).await, Err(AiError::Key));

    fake.answer(
        200,
        json!({ "stop_reason": "end_turn", "content": [{ "type": "text", "text": "not json" }] }),
    );
    assert!(matches!(ai.ask(ask()).await, Err(AiError::Failed(_))));

    let s = ai.status();
    assert_eq!(s["refused"], 1);
    assert_eq!(s["failed"], 3);
}

#[tokio::test]
async fn an_overload_is_retried_once() {
    let fake = Fake::default();
    let base = fake.start().await;
    let ai = ai(&base, "claude-opus-5-5", 10);
    let input = json!({});
    let ask = || Ask {
        system: "s",
        schema: simple_schema(),
        effort: "low",
        input: &input,
    };

    fake.answer(529, json!({ "error": { "message": "overloaded" } }));
    fake.answer(200, reply(json!({ "ok": true })));
    assert!(ai.ask(ask()).await.is_ok());
    assert_eq!(fake.seen().len(), 2);

    fake.answer(529, json!({}));
    fake.answer(429, json!({}));
    assert_eq!(ai.ask(ask()).await, Err(AiError::Busy));
    assert_eq!(fake.seen().len(), 4);
    // One call each, whatever the retries.
    assert_eq!(ai.status()["calls_today"], 2);
}

#[tokio::test]
async fn the_daily_limit_stops_calls_before_they_are_sent() {
    let fake = Fake::default();
    let base = fake.start().await;
    let ai = ai(&base, "claude-opus-5-5", 1);
    let input = json!({});
    let ask = || Ask {
        system: "s",
        schema: simple_schema(),
        effort: "low",
        input: &input,
    };
    fake.answer(200, reply(json!({ "ok": true })));
    assert!(ai.ask(ask()).await.is_ok());
    assert_eq!(ai.ask(ask()).await, Err(AiError::Limit(1)));
    assert_eq!(fake.seen().len(), 1);
    assert_eq!(AiError::Limit(1).status(), 429);
}

#[test]
fn explain_answers_are_checked_against_the_container() {
    let answer = json!({
        "verdict": "problem",
        "summary": "Meta didn't fire.",
        "findings": [
            { "severity": "problem", "title": "Wrong case", "detail": "d", "tag": "Unused", "fix": "Use purchase",
              "tag_json": "{\"name\":\"Unused\",\"template\":\"tiktok_events\",\"data\":{},\"fire_when\":[{\"key\":\"event_name\",\"op\":\"equals\",\"value\":\"purchase\"}],\"enabled\":true}" },
            { "severity": "warning", "title": "Made up", "detail": "d", "tag": "Nope", "fix": "", "tag_json": "" },
            { "severity": "info", "title": "Bad change", "detail": "d", "tag": "", "fix": "x", "tag_json": "{\"name\":\"X\",\"template\":\"t\",\"fire_when\":[{\"key\":\"a\",\"op\":\"matches_regex\",\"value\":\"(\"}]}" },
            { "severity": "info", "title": "New tag", "detail": "d", "tag": "", "fix": "x", "tag_json": "{\"name\":\"Pinterest\",\"template\":\"pinterest_capi\",\"data\":{}}" },
        ]
    });
    let v = explain::check(&answer, &container(), 7, "m");
    assert_eq!(v["trace"], 7);
    assert_eq!(v["verdict"], "problem");
    let f = v["findings"].as_array().unwrap();
    assert_eq!(f.len(), 3, "the finding about an unknown tag is dropped");
    assert_eq!(f[0]["change"]["action"], "edit");
    assert_eq!(f[0]["change"]["tag"]["fire_when"][0]["value"], "purchase");
    assert!(
        f[1]["change"].is_null(),
        "a change that doesn't load is dropped"
    );
    assert_eq!(f[2]["change"]["action"], "add");
    assert_eq!(f[2]["change"]["tag"]["enabled"], true);
}

#[tokio::test]
async fn an_explanation_is_asked_once_per_trace_and_container() {
    let fake = Fake::default();
    let base = fake.start().await;
    let ai = ai(&base, "claude-opus-5-5", 10);
    fake.answer(
        200,
        reply(json!({ "verdict": "ok", "summary": "Fine.", "findings": [] })),
    );
    let trace = json!({ "id": 3 });
    let a = explain::explain(&ai, 3, &trace, &container(), Some("tr"))
        .await
        .unwrap();
    let b = explain::explain(&ai, 3, &trace, &container(), Some("tr"))
        .await
        .unwrap();
    assert_eq!(a, b);
    assert_eq!(fake.seen().len(), 1);
    let (_, body) = fake.seen().remove(0);
    let input: Value =
        serde_json::from_str(body["messages"][0]["content"].as_str().unwrap()).unwrap();
    assert_eq!(input["language"], "Turkish");
    assert_eq!(body["output_config"]["effort"], "medium");
    assert_eq!(
        body["output_config"]["format"]["schema"]["properties"]["findings"]["items"]["required"]
            .as_array()
            .unwrap()
            .len(),
        6
    );

    let mut changed = container();
    changed["tags"][0]["enabled"] = json!(false);
    fake.answer(
        200,
        reply(json!({ "verdict": "ok", "summary": "Fine.", "findings": [] })),
    );
    explain::explain(&ai, 3, &trace, &changed, Some("tr"))
        .await
        .unwrap();
    assert_eq!(fake.seen().len(), 2, "a changed container is asked again");
}

/// One hit: its events, tag runs (tag, event index) and sends (tag, url,
/// status or error).
fn hit(
    id: u64,
    events: &[&str],
    runs: &[(&str, usize)],
    sends: &[(&str, &str, Result<u16, &str>)],
) -> (u64, Value) {
    let sends: Vec<Value> = sends
        .iter()
        .map(|(from, url, r)| {
            let result = match r {
                Ok(s) => json!({ "Ok": s }),
                Err(e) => json!({ "Err": e }),
            };
            json!({ "from": from, "method": "POST", "url": url, "result": result, "detail": if matches!(r, Ok(s) if *s >= 400) { json!("{\"error\":\"Invalid OAuth access token\"}") } else { Value::Null } })
        })
        .collect();
    (
        id,
        json!({
            "at": 1_000 + id,
            "result": {
                "events": events.iter().map(|e| json!({ "event_name": e })).collect::<Vec<_>>(),
                "tags": runs.iter().map(|(t, e)| json!({ "tag": t, "event": e, "outcome": "success", "error": null })).collect::<Vec<_>>(),
                "sends": sends,
            }
        }),
    )
}

fn sample_hits() -> Vec<(u64, Value)> {
    let ga = "https://region1.google-analytics.com/g/collect?v=2";
    let meta = "https://graph.facebook.com/v21.0/1/events";
    let mut hits = Vec::new();
    for id in 1..=4 {
        // page_view and scroll: GA4 and Old GA4 both send each, Meta sends for both.
        hits.push(hit(
            id,
            &["page_view", "scroll"],
            &[
                ("GA4", 0),
                ("GA4", 1),
                ("Old GA4", 0),
                ("Old GA4", 1),
                ("Meta", 0),
                ("Meta", 1),
            ],
            &[
                ("GA4", ga, Ok(204)),
                ("GA4", ga, Ok(204)),
                ("Old GA4", ga, Ok(204)),
                ("Old GA4", ga, Ok(204)),
                ("Meta", meta, Ok(400)),
                ("Meta", meta, Ok(400)),
            ],
        ));
    }
    // A refused hit: no result.
    hits.push((5, json!({ "at": 1_005, "status": 503, "result": null })));
    hits
}

#[test]
fn audit_counts_sends_duplicates_and_failures() {
    let c = audit::count(&sample_hits(), &container(), 7);
    let j = &c.json;
    assert_eq!(j["hits"], 5);
    assert_eq!(c.hits_ran, 4);
    assert_eq!(j["sends"], 24);
    assert_eq!(j["sends_per_1000_hits"], 6000.0);
    assert_eq!(j["events_by_name"]["scroll"], 4);
    assert_eq!(j["send_retries_since_start"], 7);

    let tag = |n: &str| {
        j["tags"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == n)
            .unwrap()
            .clone()
    };
    assert_eq!(tag("Unused")["never_fired"], true);
    assert_eq!(tag("Meta")["sends_failed"], 8);
    assert_eq!(tag("Meta")["sends_by_event"]["scroll"], 4.0);
    assert_eq!(tag("GA4")["fired_for"]["page_view"], 4);

    let dup = &j["duplicate_sends"][0];
    assert_eq!(dup["host"], "region1.google-analytics.com");
    assert_eq!(dup["tags"], json!(["GA4", "Old GA4"]));
    assert_eq!(dup["count"], 4);
    assert_eq!(dup["sample_trace_ids"], json!([1, 2, 3]));

    let fail = &j["failing_sends"][0];
    assert_eq!(fail["tag"], "Meta");
    assert_eq!(fail["result"], "400");
    assert_eq!(fail["count"], 8);
    assert!(fail["vendor_answer"].as_str().unwrap().contains("OAuth"));
}

#[test]
fn audit_savings_come_from_the_counts_not_the_model() {
    let hits = sample_hits();
    let c = audit::count(&hits, &container(), 0);
    let answer = json!({
        "summary": "Two GA4 tags send everything twice.",
        "findings": [
            { "kind": "duplicate_send", "severity": "warning", "title": "Remove Old GA4", "detail": "d", "tag": "Old GA4", "events": [], "change": "remove_tag", "fix": "Delete it", "tag_json": "", "trace_ids": [1, 2, 999] },
            { "kind": "noisy_event", "severity": "info", "title": "Meta gets scroll", "detail": "d", "tag": "Meta", "events": ["scroll", "made_up"], "change": "stop_for_events", "fix": "Only page_view",
              "tag_json": "{\"name\":\"Meta\",\"template\":\"meta_capi\",\"data\":{\"pixel_id\":\"1\",\"access_token\":\"[redacted]\"},\"fire_when\":[{\"key\":\"event_name\",\"op\":\"not_equals\",\"value\":\"scroll\"}]}", "trace_ids": [] },
            { "kind": "other", "severity": "info", "title": "Ghost", "detail": "d", "tag": "Ghost", "events": [], "change": "remove_tag", "fix": "", "tag_json": "", "trace_ids": [] },
        ]
    });
    let v = audit::check(&answer, &container(), &c, "m");
    let f = v["findings"].as_array().unwrap();
    assert_eq!(f.len(), 2, "a finding about an unknown tag is dropped");
    assert_eq!(f[0]["change"]["action"], "remove");
    assert_eq!(f[0]["sends_saved_per_1000_hits"], 2000.0);
    assert_eq!(
        f[0]["trace_ids"],
        json!([1, 2]),
        "unknown trace ids are dropped"
    );
    let nothing = json!({ "summary": "", "findings": [{ "kind": "other", "severity": "info", "title": "t", "detail": "d", "tag": "GA4", "events": [], "change": "none", "fix": "", "tag_json": "", "trace_ids": [] }] });
    let v0 = audit::check(&nothing, &container(), &c, "m");
    assert!(
        v0["findings"][0]["sends_saved_per_1000_hits"]
            .as_f64()
            .unwrap()
            .is_sign_positive(),
        "no -0.0"
    );
    assert_eq!(f[1]["events"], json!(["scroll"]));
    assert_eq!(f[1]["sends_saved_per_1000_hits"], 1000.0);
    assert_eq!(f[1]["change"]["action"], "edit");
    assert_eq!(v["counts"]["sends_per_1000_hits"], 6000.0);
    assert_eq!(v["counts"]["sends_per_1000_hits_after"], 3000.0);
}

#[tokio::test]
async fn an_audit_without_hits_asks_nothing() {
    let fake = Fake::default();
    let base = fake.start().await;
    let ai = ai(&base, "claude-opus-5-5", 10);
    let only_refused = vec![(1, json!({ "result": null }))];
    assert_eq!(
        audit::audit(&ai, &only_refused, &container(), 0, None).await,
        Err(AiError::NoHits)
    );
    assert!(fake.seen().is_empty());
}

#[test]
fn the_ai_never_gets_the_visitors_ip() {
    let detail = json!({
        "request": {
            "method": "POST",
            "query": "v=2&uip=203.0.113.9&en=page_view",
            "headers": [["x-forwarded-for", "203.0.113.9"], ["user-agent", "UA"], ["X-Real-IP", "203.0.113.9"]],
            "body": format!("_uip=203.0.113.9&en=scroll\n{}", "a".repeat(10_000)),
            "remote_address": "203.0.113.9:5555",
        },
        "result": {
            "events": [{ "event_name": "page_view", "ip_override": "203.0.113.9", "nested": { "client_ip": "203.0.113.9" } }],
            "logs": (0..150).map(|i| json!(format!("line {i}"))).collect::<Vec<_>>(),
        }
    });
    let t = crate::traces::for_ai(&detail);
    let text = t.to_string();
    assert!(!text.contains("203.0.113.9"), "{text}");
    assert_eq!(t["request"]["query"], "v=2&en=page_view");
    assert_eq!(t["request"]["headers"], json!([["user-agent", "UA"]]));
    assert!(
        t["request"]["body"]
            .as_str()
            .unwrap()
            .starts_with("en=scroll\n")
    );
    assert_eq!(t["request"]["body"].as_str().unwrap().len(), 8 * 1024);
    assert_eq!(t["request"]["body_truncated"], true);
    assert_eq!(t["result"]["events"][0]["ip_override"], "[removed]");
    assert_eq!(t["result"]["logs"].as_array().unwrap().len(), 100);
    assert_eq!(t["result"]["logs"][0], "line 50");
}

#[tokio::test]
async fn the_console_routes_explain_and_audit() {
    use crate::admin::{Admin, router};
    use crate::container::Container;
    use crate::container_file::ContainerFile;
    use crate::request::IncomingRequest;
    use crate::sandbox::RunResult;
    use crate::server::Target;
    use crate::traces::{Outcome, Traces, capture};

    let fake = Fake::default();
    let base = fake.start().await;
    let traces = Arc::new(Traces::new(10));
    let result = RunResult {
        events: vec![json!({ "event_name": "page_view", "ip_override": "198.51.100.4" })],
        ..Default::default()
    };
    let req = IncomingRequest {
        method: "GET".into(),
        path: "/g/collect".into(),
        headers: vec![("x-forwarded-for".into(), "198.51.100.4".into())],
        ..Default::default()
    };
    traces.record(
        None,
        capture(&req),
        std::time::Duration::from_millis(1),
        Outcome::Ran(&result),
    );
    let token = "t".repeat(32);
    let admin = Admin {
        pool: crate::server::Pool::for_tests(),
        log: Default::default(),
        tenants: None,
        integrations: None,
        token: token.clone(),
        traces,
        target: Target::Single(ContainerFile::fixed(
            Container::builtin(),
            Default::default(),
            Default::default(),
        )),
        workers: 1,
        ai: Some(Arc::new(ai(&base, "claude-opus-5-5", 10))),
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
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let console = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router(admin)).await });
    let http = reqwest::Client::new();
    let post = |path: &str| {
        http.post(format!("{console}{path}"))
            .header("authorization", format!("Bearer {token}"))
            .send()
    };

    fake.answer(
        200,
        reply(json!({ "verdict": "ok", "summary": "All tags fired.", "findings": [] })),
    );
    let res = post("/admin/api/ai/explain/1?lang=en").await.unwrap();
    assert_eq!(res.status(), 200);
    let v: Value = serde_json::from_str(&res.text().await.unwrap()).unwrap();
    assert_eq!(v["summary"], "All tags fired.");
    let (_, body) = fake.seen().remove(0);
    assert!(!body.to_string().contains("198.51.100.4"));

    assert_eq!(post("/admin/api/ai/explain/9").await.unwrap().status(), 404);

    fake.answer(
        200,
        reply(json!({ "summary": "Nothing to change.", "findings": [] })),
    );
    let res = post("/admin/api/ai/audit").await.unwrap();
    assert_eq!(res.status(), 200);
    let v: Value = serde_json::from_str(&res.text().await.unwrap()).unwrap();
    assert_eq!(v["counts"]["hits_ran"], 1);
    assert_eq!(fake.seen()[1].1["output_config"]["effort"], "high");

    let o: Value = serde_json::from_str(
        &http
            .get(format!("{console}/admin/api/overview"))
            .header("authorization", format!("Bearer {}", "t".repeat(32)))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(o["ai"]["calls_today"], 2);
    assert_eq!(o["ai"]["model"], "claude-opus-5-5");
}

#[test]
fn the_catalog_lists_keys_examples_and_array_items_without_ips() {
    let events = vec![
        json!({ "event_name": "purchase", "value": 49.9, "ip_override": "203.0.113.9", "items": [{ "item_id": "A", "price": 10 }], "user_data": { "address": { "city": "Berlin" } } }),
        json!({ "event_name": "page_view", "value": 0 }),
    ];
    let c = rules::catalog(&events);
    let k = &c["keys"];
    assert_eq!(c["events"], 2);
    assert_eq!(k["event_name"]["count"], 2);
    assert_eq!(
        k["event_name"]["examples"],
        json!(["purchase", "page_view"])
    );
    assert_eq!(k["items[].item_id"]["examples"], json!(["A"]));
    assert_eq!(k["items"]["count"], 1);
    assert_eq!(k["user_data.address.city"]["examples"], json!(["Berlin"]));
    assert_eq!(k["ip_override"]["examples"], json!([]));
    assert_eq!(c["event_names"], json!({ "page_view": 1, "purchase": 1 }));
}

fn rule_answer(rule: Value) -> Value {
    reply(
        json!({ "explanation": "Lower-cases event names.", "rule_json": rule.to_string(), "question": "" }),
    )
}

#[tokio::test]
async fn a_rule_that_doesnt_load_goes_back_once_with_the_error() {
    let fake = Fake::default();
    let base = fake.start().await;
    let ai = ai(&base, "claude-opus-5-5", 10);
    let check = |rule: Value| -> Result<(Value, Value), String> {
        let mut r: Vec<crate::transform::Transformation> =
            vec![serde_json::from_value(rule).map_err(|e| e.to_string())?];
        crate::transform::compile(&mut r, &["GA4"])?;
        Ok((
            serde_json::to_value(&r[0]).unwrap(),
            json!({ "matched": 0 }),
        ))
    };
    let good = json!({ "name": "Lower", "steps": [{ "op": "format", "key": "event_name", "format": "lower" }] });

    fake.answer(200, rule_answer(json!({ "name": "Lower", "tags": ["Meta"], "steps": [{ "op": "format", "key": "event_name", "format": "lower" }] })));
    fake.answer(200, rule_answer(good.clone()));
    let v = rules::write(
        &ai,
        "lower-case event names",
        &container(),
        &json!({}),
        None,
        check,
    )
    .await
    .unwrap();
    assert_eq!(v["rule"]["name"], "Lower");
    assert_eq!(v["rule"]["enabled"], true);
    let seen = fake.seen();
    assert_eq!(seen.len(), 2);
    let second: Value =
        serde_json::from_str(seen[1].1["messages"][0]["content"].as_str().unwrap()).unwrap();
    assert!(
        second["error"].as_str().unwrap().contains("unknown tag"),
        "{second}"
    );
    assert!(
        second["previous_attempt"]
            .as_str()
            .unwrap()
            .contains("Meta")
    );

    fake.answer(200, rule_answer(json!({ "name": "Bad", "steps": [] })));
    fake.answer(200, rule_answer(json!({ "name": "Bad", "steps": [] })));
    assert!(
        matches!(rules::write(&ai, "x", &container(), &json!({}), None, check).await, Err(AiError::Failed(m)) if m.contains("doesn't load"))
    );

    fake.answer(200, reply(json!({ "explanation": "", "rule_json": "", "question": "Per-item changes aren't possible; I can set a margin from item_category on the event." })));
    let v = rules::write(
        &ai,
        "add margin to each item",
        &container(),
        &json!({}),
        Some("en"),
        check,
    )
    .await
    .unwrap();
    assert!(v["rule"].is_null());
    assert!(v["question"].as_str().unwrap().contains("Per-item"));
}

#[tokio::test]
async fn the_console_route_writes_and_previews_a_rule() {
    use crate::admin::{Admin, router};
    use crate::container::Container;
    use crate::container_file::ContainerFile;
    use crate::request::IncomingRequest;
    use crate::sandbox::RunResult;
    use crate::server::Target;
    use crate::traces::{Outcome, Traces, capture};

    let fake = Fake::default();
    let base = fake.start().await;
    let traces = Arc::new(Traces::new(10));
    for name in ["Purchase", "page_view"] {
        let result = RunResult {
            events: vec![json!({ "event_name": name, "ip_override": "198.51.100.4" })],
            ..Default::default()
        };
        let req = IncomingRequest {
            method: "GET".into(),
            path: "/g/collect".into(),
            ..Default::default()
        };
        traces.record(
            None,
            capture(&req),
            std::time::Duration::from_millis(1),
            Outcome::Ran(&result),
        );
    }
    let token = "t".repeat(32);
    let admin = Admin {
        pool: crate::server::Pool::for_tests(),
        log: Default::default(),
        tenants: None,
        integrations: None,
        token: token.clone(),
        traces,
        target: Target::Single(ContainerFile::fixed(
            Container::builtin(),
            Default::default(),
            Default::default(),
        )),
        workers: 1,
        ai: Some(Arc::new(ai(&base, "claude-opus-5-5", 10))),
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
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let console = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router(admin)).await });
    let http = reqwest::Client::new();

    fake.answer(200, rule_answer(json!({ "name": "Lower", "when": [{ "key": "event_name", "op": "equals", "value": "Purchase" }], "steps": [{ "op": "format", "key": "event_name", "format": "lower" }] })));
    let res = http
        .post(format!("{console}/admin/api/ai/transformation?lang=en"))
        .header("authorization", format!("Bearer {token}"))
        .body(json!({ "request": "Purchase should be purchase" }).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let v: Value = serde_json::from_str(&res.text().await.unwrap()).unwrap();
    assert_eq!(v["preview"]["events"], 2);
    assert_eq!(v["preview"]["changed"], 1);
    assert_eq!(
        v["preview"]["samples"][0]["changes"][0]["after"],
        "purchase"
    );
    let (_, body) = fake.seen().remove(0);
    let input: Value =
        serde_json::from_str(body["messages"][0]["content"].as_str().unwrap()).unwrap();
    assert_eq!(input["event_keys"]["keys"]["event_name"]["count"], 2);
    assert!(!body.to_string().contains("198.51.100.4"));

    let res = http
        .post(format!("{console}/admin/api/container/transformations/preview"))
        .header("authorization", format!("Bearer {token}"))
        .body(json!({ "rule": { "name": "Bad", "tags": ["Nope"], "steps": [{ "op": "remove", "keys": ["x"] }] } }).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
}

#[tokio::test]
async fn a_change_review_and_an_alert_diagnosis_ask_once_each() {
    let fake = Fake::default();
    let base = fake.start().await;
    let ai = ai(&base, "claude-opus-5-5", 10);
    fake.answer(200, reply(json!({ "verdict": "dangerous", "summary": "Meta stops getting purchases.", "concerns": ["Meta: purchase"] })));
    let edits = json!([{ "kind": "tag_changed", "name": "Meta", "fields": ["fire_when"] }]);
    let replay = json!({ "hits": 3, "changed_hits": 3, "groups": [{ "tag": "Meta", "kind": "stops", "event": "purchase", "hits": 3 }] });
    let v = review::review(&ai, &edits, &replay, Some("tr"))
        .await
        .unwrap();
    assert_eq!(v["verdict"], "check", "an unknown verdict becomes check");
    assert_eq!(v["model"], "claude-opus-5-5");
    let (_, body) = fake.seen().remove(0);
    let input: Value =
        serde_json::from_str(body["messages"][0]["content"].as_str().unwrap()).unwrap();
    assert_eq!(input["language"], "Turkish");
    assert_eq!(input["replay"]["groups"][0]["kind"], "stops");

    fake.answer(200, reply(json!({ "summary": "s", "likely_cause": "The Meta token expired.", "check_first": "Events Manager" })));
    let alert = json!({ "kind": "vendor_failing", "subject": "graph.facebook.com", "numbers": { "rate_pct": 60 }, "texts": { "en": { "title": "t" } }, "related": [] });
    let v = diagnose::diagnose(&ai, &alert, &[], &container(), None)
        .await
        .unwrap();
    assert_eq!(v["likely_cause"], "The Meta token expired.");
    let (_, body) = fake.seen().remove(1);
    let input: Value =
        serde_json::from_str(body["messages"][0]["content"].as_str().unwrap()).unwrap();
    assert_eq!(input["alert"]["subject"], "graph.facebook.com");
}

#[test]
fn descriptions_with_secrets_are_refused() {
    assert!(setup::looks_secret(
        "Meta token EAAGm0PX4ZCpsBAJZBZC1234567890abcdef"
    ));
    assert!(setup::looks_secret("refresh token 1//0gAbCdEf"));
    assert!(setup::looks_secret(
        "key: abcdefghijklmnopqrstuvwxyz0123456789ABCD"
    ));
    assert!(!setup::looks_secret(
        "Shopify store, GA4 G-TEST000001, Meta pixel 1234567890123456, Google Ads account 123-456-7890"
    ));
    assert!(!setup::looks_secret(
        "https://example.com/thanks for purchases"
    ));
}

#[test]
fn needs_list_empty_required_settings() {
    let tags = vec![
        json!({ "name": "Meta", "template": "meta_capi", "data": { "pixel_id": "1", "access_token": "" } }),
        json!({ "name": "Ads", "template": "google_ads", "data": { "customer_id": "1", "conversion_action_id": "2", "client_id": "c", "client_secret": "[redacted]" } }),
        json!({ "name": "GA4", "template": "ga4_tag", "data": {} }),
    ];
    let n: Vec<(String, String, bool)> = setup::needs(&tags)
        .iter()
        .map(|v| {
            (
                v["tag"].as_str().unwrap().into(),
                v["setting"].as_str().unwrap().into(),
                v["secret"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        n,
        vec![
            ("Meta".into(), "access_token".into(), true),
            ("Ads".into(), "refresh_token".into(), true),
        ]
    );
}

#[tokio::test]
async fn a_setup_that_doesnt_load_goes_back_once() {
    let fake = Fake::default();
    let base = fake.start().await;
    let ai = ai(&base, "claude-opus-5-5", 10);
    let good = json!([
        { "name": "GA4", "template": "ga4_tag", "data": {} },
        { "name": "Meta", "template": "meta_capi", "data": { "pixel_id": "123", "access_token": "" } },
    ]);
    fake.answer(200, reply(json!({ "explanation": "x", "tags_json": "[{\"name\":\"Meta\",\"template\":\"meta_pixel\"}]", "transformations_json": "", "web_steps": [], "question": "" })));
    fake.answer(200, reply(json!({ "explanation": "GA4 and Meta.", "tags_json": good.to_string(), "transformations_json": "[]", "web_steps": ["Set server_container_url."], "question": "" })));
    let check = |tags: &Value, _: &Value| -> Result<(), String> {
        let ok = tags
            .as_array()
            .unwrap()
            .iter()
            .all(|t| ["ga4_tag", "meta_capi"].contains(&t["template"].as_str().unwrap_or("")));
        if ok {
            Ok(())
        } else {
            Err("unknown template meta_pixel".into())
        }
    };
    let v = setup::propose(
        &ai,
        "GA4 and Meta pixel 123",
        &container(),
        &json!({}),
        Some("tr"),
        check,
    )
    .await
    .unwrap();
    assert_eq!(v["proposal"]["tags"], good);
    assert_eq!(v["needs"][0]["setting"], "access_token");
    assert_eq!(v["web_steps"][0], "Set server_container_url.");
    let seen = fake.seen();
    assert_eq!(seen.len(), 2);
    let second: Value =
        serde_json::from_str(seen[1].1["messages"][0]["content"].as_str().unwrap()).unwrap();
    assert_eq!(second["error"], "unknown template meta_pixel");
    assert_eq!(second["language"], "Turkish");
    assert_eq!(seen[0].1["output_config"]["effort"], "high");
    assert!(
        seen[0].1["system"][0]["text"]
            .as_str()
            .unwrap()
            .contains("google_ads (Google Ads conversions): customer_id (required)")
    );
    assert!(
        seen[0].1["system"][0]["text"]
            .as_str()
            .unwrap()
            .contains("also takes eventUserData")
    );

    fake.answer(200, reply(json!({ "explanation": "", "tags_json": "", "transformations_json": "", "web_steps": [], "question": "Which platforms?" })));
    let v = setup::propose(&ai, "set it up", &container(), &json!({}), None, check)
        .await
        .unwrap();
    assert!(v["proposal"].is_null());
    assert_eq!(v["question"], "Which platforms?");
}

#[tokio::test]
async fn a_checkup_becomes_a_two_sided_todo_list() {
    let fake = Fake::default();
    let base = fake.start().await;
    let ai = ai(&base, "claude-opus-5-5", 10);
    fake.answer(200, reply(json!({ "summary": "Hits arrive; Meta can't send.", "website": ["Send currency."], "server": ["Fill in Meta's access token."] })));
    let items = json!([{ "kind": "tag_settings", "status": "bad", "side": "server", "params": { "tag": "Meta", "settings": ["Access token"] } }]);
    let v = todo::todo(
        &ai,
        &items,
        &json!({ "pixels": ["meta"] }),
        &container(),
        Some("tr"),
    )
    .await
    .unwrap();
    assert_eq!(v["server"][0], "Fill in Meta's access token.");
    let (_, body) = fake.seen().remove(0);
    let input: Value =
        serde_json::from_str(body["messages"][0]["content"].as_str().unwrap()).unwrap();
    assert_eq!(input["findings"][0]["kind"], "tag_settings");
    assert_eq!(input["web"]["pixels"][0], "meta");
    assert_eq!(input["tags"][1]["name"], "Meta");
    assert_eq!(input["language"], "Turkish");
}
