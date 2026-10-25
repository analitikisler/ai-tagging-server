use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;

use super::*;
use crate::container::{Container, Entry, Settings, Tag};

/// Records outgoing requests and answers each with a fixed status.
struct Recorder {
    sent: Mutex<Vec<Outgoing>>,
    status: u16,
    body: String,
}

impl Transport for Recorder {
    fn send_all(&self, requests: Vec<Outgoing>) -> Vec<Result<SendResult, String>> {
        let mut sent = self.sent.lock().unwrap();
        requests
            .into_iter()
            .map(|r| {
                sent.push(r);
                Ok(SendResult {
                    status: self.status,
                    headers: vec![],
                    body: self.body.clone(),
                })
            })
            .collect()
    }
}

fn engine_with(container: Container, status: u16) -> (Engine, Arc<Recorder>) {
    engine_answering(container, status, "ok")
}

fn engine_answering(container: Container, status: u16, body: &str) -> (Engine, Arc<Recorder>) {
    let rec = Arc::new(Recorder {
        sent: Mutex::new(vec![]),
        status,
        body: body.into(),
    });
    let engine = Engine::new(
        Arc::new(container),
        Default::default(),
        rec.clone(),
        Duration::from_millis(500),
        32 * 1024 * 1024,
    )
    .unwrap();
    (engine, rec)
}

fn ga4_hit(query: &str, body: &str) -> IncomingRequest {
    IncomingRequest {
        method: if body.is_empty() {
            "GET".into()
        } else {
            "POST".into()
        },
        path: "/g/collect".into(),
        query: query.into(),
        headers: vec![
            ("user-agent".into(), "Mozilla/5.0 Test".into()),
            ("host".into(), "example.com".into()),
            ("origin".into(), "https://example.com".into()),
            (
                "cookie".into(),
                "_ga=GA1.1.111.222; _ga_ABC=GS2.1.s1$o1; other=x".into(),
            ),
        ],
        body: body.into(),
        remote_address: "203.0.113.9".into(),
    }
}

fn custom(templates: &[(&str, &str)], clients: &[&str], tags: &[&str]) -> Container {
    let mut c = Container::builtin();
    for (name, src) in templates {
        c.templates.insert(name.to_string(), src.to_string());
    }
    c.clients = clients
        .iter()
        .map(|t| Entry {
            name: t.to_string(),
            template: t.to_string(),
            data: json!({}),
        })
        .collect();
    c.tags = tags
        .iter()
        .map(|t| serde_json::from_value::<Tag>(json!({"name": t, "template": t})).unwrap())
        .collect();
    c.compile().unwrap();
    c
}

#[test]
fn ga4_hit_is_claimed_forwarded_and_answered_204() {
    let (engine, rec) = engine_with(Container::builtin(), 204);
    let r = engine.run(ga4_hit(
        "v=2&tid=G-TEST000001&cid=111.222&en=page_view&dl=https%3A%2F%2Fexample.com%2F",
        "",
    ));

    assert_eq!(r.claimed_by.as_deref(), Some("GA4"));
    assert_eq!(r.response.status, 204, "{:?}", r.logs);
    assert!(r.response.headers.contains(&(
        "access-control-allow-origin".into(),
        "https://example.com".into()
    )));

    let sent = rec.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    let url = &sent[0].url;
    assert!(
        url.starts_with("https://www.google-analytics.com/g/collect?"),
        "{url}"
    );
    assert!(
        url.contains("en=page_view") && url.contains("tid=G-TEST000001"),
        "{url}"
    );
    assert!(url.contains("_uip=203.0.113.9"), "{url}");
    assert!(url.contains("_ua=Mozilla%2F5.0%20Test"), "{url}");
    assert_eq!(r.tags[0].outcome, "success");
}

#[test]
fn the_ga4_tag_forwards_what_transformations_changed() {
    let q = "v=2&tid=G-1&cid=111.222&en=Purchase&dl=https%3A%2F%2Fexample.com%2F&ep.value=49%2C90&ep.coupon=SUMMER+SALE&ep.email=x%40y.z&epn.qty=2";
    // Untransformed: the hit goes as it was sent.
    let (engine, rec) = engine_with(Container::builtin(), 204);
    engine.run(ga4_hit(q, ""));
    let plain = rec.sent.lock().unwrap()[0].url.clone();
    assert!(plain.contains("en=Purchase&dl=https%3A%2F%2Fexample.com%2F&ep.value=49%2C90&ep.coupon=SUMMER+SALE&ep.email=x%40y.z&epn.qty=2"), "{plain}");

    let mut c = Container::builtin();
    c.transformations = serde_json::from_value(json!([{ "name": "Clean", "steps": [
        { "op": "format", "key": "event_name", "format": "lower" },
        { "op": "format", "key": "value", "format": "number" },
        { "op": "remove", "keys": ["email"] },
        { "op": "set", "key": "currency", "value": "EUR" },
        { "op": "set", "key": "margin", "value": 0.4 },
        { "op": "set", "key": "nested", "value": { "a": 1 } },
    ]}]))
    .unwrap();
    c.compile().unwrap();
    let (engine, rec) = engine_with(c, 204);
    engine.run(ga4_hit(q, ""));
    let url = rec.sent.lock().unwrap()[0].url.clone();
    assert!(url.contains("en=purchase&"), "{url}");
    assert!(url.contains("epn.value=49.9&"), "{url}");
    assert!(
        url.contains("ep.coupon=SUMMER+SALE"),
        "unchanged values stay as sent: {url}"
    );
    assert!(url.contains("epn.qty=2"), "{url}");
    assert!(!url.contains("email"), "{url}");
    assert!(
        url.contains("&cu=EUR") && url.contains("epn.margin=0.4"),
        "{url}"
    );
    assert!(
        !url.contains("nested") && !url.contains("ip_override") && !url.contains("client_id"),
        "{url}"
    );
}

#[test]
fn batched_post_runs_the_container_per_event() {
    let (engine, rec) = engine_with(Container::builtin(), 204);
    let r = engine.run(ga4_hit(
        "v=2&tid=G-1&cid=1.2",
        "en=page_view\nen=scroll&epn.percent_scrolled=90",
    ));
    assert_eq!(r.events.len(), 2);
    assert_eq!(rec.sent.lock().unwrap().len(), 2);
    assert_eq!(r.response.status, 204);
}

#[test]
fn unclaimed_request_gets_400() {
    let (engine, _) = engine_with(Container::builtin(), 204);
    let mut req = ga4_hit("", "");
    req.path = "/anything".into();
    let r = engine.run(req);
    assert_eq!(r.response.status, 400);
    assert!(r.claimed_by.is_none());
}

#[test]
fn failing_vendor_marks_the_tag_failed_but_still_answers() {
    let (engine, _) = engine_with(Container::builtin(), 500);
    let r = engine.run(ga4_hit("v=2&en=page_view", ""));
    assert_eq!(r.tags[0].outcome, "failure");
    assert_eq!(r.response.status, 204);
}

fn set_cookies(r: &RunResult) -> Vec<String> {
    r.response
        .headers
        .iter()
        .filter(|(k, _)| k == "set-cookie")
        .map(|(_, v)| v.clone())
        .collect()
}

#[test]
fn extends_ga_cookies_by_default_unless_analytics_is_denied() {
    let (engine, _) = engine_with(Container::builtin(), 204);
    let r = engine.run(ga4_hit("v=2&tid=G-ABC&en=page_view&gcs=G111", ""));
    assert_eq!(
        set_cookies(&r),
        vec![
            "_ga=GA1.1.111.222; Domain=example.com; Path=/; Max-Age=63072000; Secure; SameSite=Lax",
            "_ga_ABC=GS2.1.s1$o1; Domain=example.com; Path=/; Max-Age=63072000; Secure; SameSite=Lax",
        ]
    );

    let r = engine.run(ga4_hit("v=2&tid=G-ABC&en=page_view&gcs=G100", ""));
    assert!(set_cookies(&r).is_empty());

    let mut c = Container::builtin();
    c.clients[0].data = json!({"extendGaCookie": false});
    let (engine, _) = engine_with(c, 204);
    let r = engine.run(ga4_hit("v=2&tid=G-ABC&en=page_view&gcs=G111", ""));
    assert!(set_cookies(&r).is_empty());
}

fn with_client_data(data: serde_json::Value) -> Container {
    let mut c = Container::builtin();
    c.clients[0].data = data;
    c
}

#[test]
fn keeps_platform_cookies_by_group_and_consent() {
    let (engine, _) = engine_with(
        with_client_data(json!({"keepCookies": ["meta", "google_ads", "my_id"]})),
        204,
    );
    let hit = |gcs: &str| {
        with_header(
            ga4_hit(&format!("v=2&tid=G-ABC&en=page_view&gcs={gcs}"), ""),
            "cookie",
            Some("_ga=GA1.1.1.2; _fbp=fb.1.1.2; _gcl_aw=GCL.1.abc; my_id=7; other=x"),
        )
    };
    let names = |r: &RunResult| -> Vec<String> {
        set_cookies(r)
            .iter()
            .map(|c| c.split('=').next().unwrap().to_string())
            .collect()
    };
    let r = engine.run(hit("G111"));
    assert_eq!(names(&r), ["_ga", "_fbp", "_gcl_aw", "my_id"]);
    assert!(set_cookies(&r).iter().any(|c| c
        == "_fbp=fb.1.1.2; Domain=example.com; Path=/; Max-Age=33696000; Secure; SameSite=Lax"));
    // Ad storage denied: only the analytics cookie.
    assert_eq!(names(&engine.run(hit("G101"))), ["_ga"]);
    // Analytics denied: ad cookies, but not _ga or a cookie of unknown purpose.
    assert_eq!(names(&engine.run(hit("G110"))), ["_fbp", "_gcl_aw"]);
}

#[test]
fn restores_aliased_click_ids_in_page_location() {
    let (engine, rec) = engine_with(
        with_client_data(json!({"clickIdAliases": {"gclid": "gc_id", "msclkid": "ms_id"}})),
        204,
    );
    let dl = |url: &str| {
        format!(
            "v=2&tid=G-TEST000001&cid=1.2&en=page_view&dl={}",
            urlencoding(url)
        )
    };
    let r = engine.run(ga4_hit(&dl("https://example.com/p?gc_id=ABC&x=1#top"), ""));
    assert_eq!(
        r.events[0]["page_location"],
        "https://example.com/p?gc_id=ABC&x=1&gclid=ABC#top"
    );
    let sent = rec.sent.lock().unwrap()[0].url.clone();
    assert!(sent.contains("gclid%3DABC"), "{sent}");

    // A click id that's already there wins over its alias.
    let r = engine.run(ga4_hit(
        &dl("https://example.com/?gclid=REAL&gc_id=ABC"),
        "",
    ));
    assert_eq!(
        r.events[0]["page_location"],
        "https://example.com/?gclid=REAL&gc_id=ABC"
    );
}

fn urlencoding(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn ad_cookies(r: &RunResult) -> Vec<String> {
    set_cookies(r)
        .into_iter()
        .filter(|c| !c.starts_with("_ga"))
        .collect()
}

#[test]
fn visitor_info_is_added_for_tags_but_not_forwarded() {
    let hit = || {
        let req = ga4_hit(
            &format!(
                "v=2&tid=G-TEST000001&cid=1.2&en=page_view&dl={}&dr={}",
                urlencoding("https://example.com/"),
                urlencoding("https://chatgpt.com/")
            ),
            "",
        );
        let req = with_header(
            req,
            "user-agent",
            Some(
                "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 Version/18.0 Mobile/15E148 Safari/604.1",
            ),
        );
        let req = with_header(req, "x-sst-geo-country", Some("TR"));
        with_header(req, "x-sst-geo-city", Some("%C4%B0stanbul"))
    };

    let (engine, _) = engine_with(Container::builtin(), 204);
    let r = engine.run(hit());
    assert!(r.events[0].get("x-sst-country").is_none(), "off by default");

    let (engine, rec) = engine_with(with_client_data(json!({"visitorInfo": true})), 204);
    let r = engine.run(hit());
    let e = &r.events[0];
    assert_eq!(e["x-sst-country"], "TR");
    assert_eq!(e["x-sst-city"], "İstanbul");
    assert_eq!(e["x-sst-device"], "mobile");
    assert_eq!(e["x-sst-browser"], "Safari");
    assert_eq!(e["x-sst-os"], "iOS");
    assert_eq!(e["x-sst-ai-source"], "chatgpt");
    assert!(e.get("x-sst-ai-agent").is_none());
    let sent = rec.sent.lock().unwrap()[0].url.clone();
    assert!(!sent.contains("sst"), "{sent}");

    // An agent browsing in a real browser, signed as ChatGPT's.
    let agent_hit = || {
        ga4_hit(
            "v=2&tid=G-TEST000001&cid=1.2&en=page_view&dl=https%3A%2F%2Fexample.com%2F%3Futm_source%3Dperplexity.ai",
            "",
        )
    };
    let r = engine.run(with_header(
        agent_hit(),
        "signature-agent",
        Some("\"https://chatgpt.com\""),
    ));
    assert_eq!(r.events[0]["x-sst-ai-agent"], "chatgpt");
    assert_eq!(r.events[0]["x-sst-ai-source"], "perplexity");

    // ChatGPT-User says "bot" in its user agent: dropped unless bots are flagged.
    let chatgpt_user = "Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko); compatible; ChatGPT-User/1.0; +https://openai.com/bot";
    let r = engine.run(with_header(agent_hit(), "user-agent", Some(chatgpt_user)));
    assert!(r.events.is_empty());
    let (engine, _) = engine_with(
        with_client_data(json!({"visitorInfo": true, "botHandling": "flag"})),
        204,
    );
    let r = engine.run(with_header(agent_hit(), "user-agent", Some(chatgpt_user)));
    assert_eq!(r.events[0]["x-sst-ai-agent"], "chatgpt");
    assert_eq!(r.events[0]["x-sst-bot"], true);
}

#[test]
fn flagged_bots_are_forwarded_with_a_score() {
    let (engine, rec) = engine_with(with_client_data(json!({"botHandling": "flag"})), 204);
    let r = engine.run(with_header(
        ga4_hit("v=2&tid=G-TEST000001&cid=1.2&en=page_view", ""),
        "user-agent",
        Some("Mozilla/5.0 (compatible; Googlebot/2.1)"),
    ));
    assert_eq!(r.events[0]["x-sst-bot"], true);
    assert_eq!(r.events[0]["x-sst-bot-score"], 100);
    assert_eq!(r.events[0]["x-sst-bot-reason"], "user_agent");
    assert_eq!(rec.sent.lock().unwrap().len(), 1, "forwarded, not dropped");

    // A browser that sends what browsers send scores 0.
    let req = with_header(
        ga4_hit("v=2&tid=G-TEST000001&cid=1.2&en=page_view", ""),
        "user-agent",
        Some("Mozilla/5.0 (Windows NT 10.0) AppleWebKit/537.36 Chrome/140.0 Safari/537.36"),
    );
    let req = with_header(req, "accept-language", Some("en-US"));
    let r = engine.run(with_header(req, "sec-fetch-mode", Some("no-cors")));
    assert_eq!(r.events[0]["x-sst-bot"], false);
    assert_eq!(r.events[0]["x-sst-bot-score"], 0);

    // Chrome without the headers every Chrome sends looks automated.
    let r = engine.run(with_header(
        ga4_hit("v=2&tid=G-TEST000001&cid=1.2&en=page_view", ""),
        "user-agent",
        Some("Mozilla/5.0 (Windows NT 10.0) AppleWebKit/537.36 Chrome/140.0 Safari/537.36"),
    ));
    assert_eq!(r.events[0]["x-sst-bot"], true);
    assert_eq!(
        r.events[0]["x-sst-bot-reason"],
        "no_accept_language,no_fetch_metadata"
    );
}

#[test]
fn keeps_ad_cookies_only_when_ad_storage_is_granted() {
    let (engine, _) = engine_with(Container::builtin(), 204);
    let cookie = "_fbp=fb.1.1.42; _fbc=fb.1.5.OLD; _ttp=tt1";
    let attrs = "; Domain=example.com; Path=/; Max-Age=33696000; Secure; SameSite=Lax";

    let r = engine.run(with_cookie(
        ga4_hit("v=2&tid=G-ABC&en=page_view&gcs=G111", ""),
        cookie,
    ));
    assert_eq!(
        ad_cookies(&r),
        vec![
            format!("_fbp=fb.1.1.42{attrs}"),
            format!("_fbc=fb.1.5.OLD{attrs}"),
            format!("_ttp=tt1{attrs}"),
        ]
    );

    // gcd wins over gcs: ad_storage denied there.
    let r = engine.run(with_cookie(
        ga4_hit("v=2&tid=G-ABC&en=page_view&gcs=G111&gcd=13p3r3r2r5l1", ""),
        cookie,
    ));
    assert!(ad_cookies(&r).is_empty());
    let r = engine.run(with_cookie(
        ga4_hit("v=2&tid=G-ABC&en=page_view&gcs=G101", ""),
        cookie,
    ));
    assert!(ad_cookies(&r).is_empty());
    let r = engine.run(with_cookie(
        ga4_hit("v=2&tid=G-ABC&en=page_view", ""),
        cookie,
    ));
    assert!(ad_cookies(&r).is_empty());

    let mut c = Container::builtin();
    c.clients[0].data = json!({"keepAdCookies": false});
    let (engine, _) = engine_with(c, 204);
    let r = engine.run(with_cookie(
        ga4_hit("v=2&tid=G-ABC&en=page_view&gcs=G111", ""),
        cookie,
    ));
    assert!(ad_cookies(&r).is_empty());
}

#[test]
fn creates_fbc_from_fbclid_on_the_landing_hit() {
    let (engine, _) = engine_with(Container::builtin(), 204);
    let dl = "dl=https%3A%2F%2Fexample.com%2F%3Ffbclid%3DABC";
    let r = engine.run(ga4_hit(
        &format!("v=2&tid=G-ABC&en=page_view&gcd=13t3t3r2r5l1&{dl}"),
        "",
    ));
    let c = ad_cookies(&r);
    assert_eq!(c.len(), 1, "{c:?}");
    assert!(
        c[0].starts_with("_fbc=fb.1.") && c[0].contains(".ABC;"),
        "{c:?}"
    );

    // Same click id: the existing cookie is kept as is.
    let r = engine.run(with_cookie(
        ga4_hit(&format!("v=2&tid=G-ABC&en=page_view&gcs=G111&{dl}"), ""),
        "_fbc=fb.1.5.ABC",
    ));
    assert!(ad_cookies(&r)[0].starts_with("_fbc=fb.1.5.ABC;"));

    // An fbclid carrying cookie attributes isn't one: no _fbc from it.
    let r = engine.run(ga4_hit(
        "v=2&tid=G-ABC&en=page_view&gcd=13t3t3r2r5l1&dl=https%3A%2F%2Fexample.com%2F%3Ffbclid%3DX%253B%2520HttpOnly",
        "",
    ));
    assert!(
        !ad_cookies(&r).iter().any(|c| c.starts_with("_fbc=")),
        "{:?}",
        ad_cookies(&r)
    );
}

#[test]
fn templates_get_no_ambient_host_access() {
    let client = r#"
        const claimRequest = require('claimRequest');
        const log = require('logToConsole');
        claimRequest();
        log(typeof host, typeof eval);
        let fnCtor = 'blocked';
        try { fnCtor = (function () {}).constructor('return 1')(); } catch (e) { fnCtor = 'threw'; }
        log('ctor', fnCtor);
        try { require('__secret'); } catch (e) { log(e.message); }
        require('returnResponse')();
    "#;
    let (engine, _) = engine_with(custom(&[("c", client)], &["c"], &[]), 204);
    let r = engine.run(ga4_hit("", ""));
    assert!(
        r.logs.contains(&"[c] undefined undefined".to_string()),
        "{:?}",
        r.logs
    );
    assert!(
        r.logs.contains(&"[c] ctor threw".to_string()),
        "{:?}",
        r.logs
    );
    assert!(
        r.logs.iter().any(|l| l.contains("is not a permitted API")),
        "{:?}",
        r.logs
    );
}

#[test]
fn infinite_loop_is_interrupted() {
    let client = "require('claimRequest')(); while (true) {}";
    let (engine, _) = engine_with(custom(&[("c", client)], &["c"], &[]), 204);
    let r = engine.run(ga4_hit("", ""));
    assert_eq!(r.response.status, 500);
    assert!(r.client_error.is_some());
}

#[test]
fn event_data_cookies_and_helpers() {
    let client = r#"
        require('claimRequest')();
        require('runContainer')({event_name: 'x', user: {id: 'u1'}, n: 3}, () => require('returnResponse')());
    "#;
    let tag = r#"
        const log = require('logToConsole');
        const getEventData = require('getEventData');
        log(getEventData('event_name'), getEventData('user.id'), getEventData('missing.key'));
        log(require('getCookieValues')('_ga')[0], require('getCookieValues')('nope').length);
        log(require('computeEffectiveTldPlusOne')('https://a.b.example.co.uk/x'));
        log(require('sha256Sync')('abc', {outputEncoding: 'hex'}));
        log(require('fromBase64')(require('toBase64')('hello')));
        log(require('parseUrl')('https://user@ex.com:8080/p?q=1&q=2#h').searchParams.q.length);
        log(require('makeInteger')('7.9'), require('getType')([]), require('makeString')({a: 1}));
        const s = require('templateDataStorage');
        s.setItemCopy('k', {v: 1});
        log(s.getItemCopy('k').v);
        data.gtmOnSuccess();
    "#;
    let (engine, _) = engine_with(custom(&[("c", client), ("t", tag)], &["c"], &["t"]), 204);
    let r = engine.run(ga4_hit("", ""));
    let expected = [
        "[t] x u1 undefined",
        "[t] GA1.1.111.222 0",
        "[t] example.co.uk",
        "[t] ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        "[t] hello",
        "[t] 2",
        "[t] 7 array {\"a\":1}",
        "[t] 1",
    ];
    for line in expected {
        assert!(
            r.logs.contains(&line.to_string()),
            "missing {line:?} in {:?}",
            r.logs
        );
    }
    assert_eq!(r.tags[0].outcome, "success");
}

#[test]
fn firing_rules_pick_tags_per_event() {
    let client = r#"
        require('claimRequest')();
        const run = require('runContainer');
        run({event_name: 'page_view'});
        run({event_name: 'purchase'}, () => require('returnResponse')());
    "#;
    let tag = "data.gtmOnSuccess();";
    let mut c = custom(&[("c", client), ("t", tag)], &["c"], &[]);
    c.tags = vec![
        serde_json::from_value(json!({
            "name": "purchases only", "template": "t",
            "fire_when": [{"key": "event_name", "op": "equals", "value": "purchase"}]
        }))
        .unwrap(),
    ];
    c.compile().unwrap();
    let (engine, _) = engine_with(c, 204);
    let r = engine.run(ga4_hit("", ""));
    assert_eq!(r.tags.len(), 1);
    assert_eq!(r.tags[0].event, 1);
}

#[test]
fn tags_fire_on_and_read_the_event_their_transformations_made() {
    let client = r#"
        require('claimRequest')();
        require('runContainer')({event_name: 'Purchase', value: '49,90', email: 'x@y'}, () => require('returnResponse')());
    "#;
    let tag = r#"
        const get = require('getEventData');
        require('logToConsole')(get('event_name'), get('value'), get('email'));
        data.gtmOnSuccess();
    "#;
    let mut c = custom(&[("c", client), ("t", tag)], &["c"], &[]);
    c.tags = serde_json::from_value(json!([
        { "name": "Shop", "template": "t", "fire_when": [{"key": "event_name", "op": "equals", "value": "purchase"}] },
        { "name": "Raw", "template": "t" },
    ]))
    .unwrap();
    c.transformations = serde_json::from_value(json!([
        { "name": "Clean", "tags": ["Shop"], "steps": [
            { "op": "format", "key": "event_name", "format": "lower" },
            { "op": "format", "key": "value", "format": "number" },
            { "op": "remove", "keys": ["email"] },
        ]},
    ]))
    .unwrap();
    c.compile().unwrap();
    let (engine, _) = engine_with(c.clone(), 204);
    let r = engine.run(ga4_hit("", ""));
    assert_eq!(r.tags.len(), 2, "{:?}", r.logs);
    assert!(
        r.logs
            .contains(&"[Shop] purchase 49.9 undefined".to_string()),
        "{:?}",
        r.logs
    );
    assert!(
        r.logs.contains(&"[Raw] Purchase 49,90 x@y".to_string()),
        "{:?}",
        r.logs
    );
    assert_eq!(
        r.events[0]["event_name"], "Purchase",
        "the record keeps the client's event"
    );
    let t = c.triggers(&r.events);
    assert!(t[0].fired);
    assert_eq!(t[0].transformed_by, vec!["Clean"]);
    assert!(t[1].transformed_by.is_empty());
}

#[test]
fn tag_that_never_reports_does_not_hang_the_client() {
    let client = r#"
        require('claimRequest')();
        require('runContainer')({event_name: 'x'}, () => require('returnResponse')());
    "#;
    let (engine, _) = engine_with(
        custom(&[("c", client), ("t", "// does nothing")], &["c"], &["t"]),
        204,
    );
    let r = engine.run(ga4_hit("", ""));
    assert_eq!(r.response.status, 200);
    assert_eq!(r.tags[0].outcome, "failure");
}

#[test]
fn full_template_storage_still_takes_new_keys() {
    // Fills the map past its cap, then checks the newest keys were kept.
    let client = r#"
        require('claimRequest')();
        const s = require('templateDataStorage');
        const log = require('logToConsole');
        for (let i = 0; i < 10005; i++) s.setItemCopy('k' + i, i);
        log('last=' + s.getItemCopy('k10004'));
        require('returnResponse')();
    "#;
    let (engine, _) = engine_with(custom(&[("c", client)], &["c"], &[]), 204);
    let r = engine.run(ga4_hit("", ""));
    assert!(
        r.logs.contains(&"[c] last=10004".to_string()),
        "{:?}",
        r.logs
    );
    let storage = engine.storage.lock().unwrap();
    assert_eq!(storage["c"].len(), 10_000);
}

#[test]
fn identical_gets_of_one_event_go_out_once_but_posts_do_not() {
    let tag = r#"
        const send = require('sendHttpRequest');
        let left = 2;
        const done = (r) => { if (--left === 0) data.gtmOnSuccess(); };
        send('https://a.example/x', {method: 'GET'}).then(done);
        send('https://a.example/x', {method: 'POST'}, 'b').then(done);
    "#;
    // Two identical events: each sends its own GET, shared by its two tags.
    let client = r#"
        require('claimRequest')();
        const run = require('runContainer');
        let left = 2;
        const done = () => { if (--left === 0) require('returnResponse')(); };
        run({event_name: 'x'}, done);
        run({event_name: 'x'}, done);
    "#;
    let (engine, rec) = engine_with(
        custom(
            &[("c", client), ("t1", tag), ("t2", tag)],
            &["c"],
            &["t1", "t2"],
        ),
        200,
    );
    let r = engine.run(ga4_hit("", ""));
    assert!(
        r.tags.iter().all(|t| t.outcome == "success"),
        "{:?} {:?}",
        r.tags,
        r.logs
    );
    let sent = rec.sent.lock().unwrap();
    assert_eq!(sent.iter().filter(|o| o.method == "GET").count(), 2);
    assert_eq!(sent.iter().filter(|o| o.method == "POST").count(), 4);
}

#[test]
fn engine_is_reusable_and_runs_do_not_share_state() {
    let client = r#"
        require('claimRequest')();
        globalThis.counter = (globalThis.counter || 0) + 1;
        require('logToConsole')(globalThis.counter);
        require('returnResponse')();
    "#;
    let (engine, _) = engine_with(custom(&[("c", client)], &["c"], &[]), 204);
    for _ in 0..3 {
        let r = engine.run(ga4_hit("", ""));
        assert!(r.logs.contains(&"[c] 1".to_string()), "{:?}", r.logs);
    }
}

fn script_request(path: &str, query: &str) -> IncomingRequest {
    IncomingRequest {
        method: "GET".into(),
        path: path.into(),
        query: query.into(),
        headers: vec![("host".into(), "example.com".into())],
        body: String::new(),
        remote_address: "203.0.113.9".into(),
    }
}

#[test]
fn serves_allowed_google_scripts_and_caches_them() {
    let c = Container::builtin_with_scripts(&["GTM-ABC".into(), "G-XYZ".into()]);
    let (engine, rec) = engine_with(c, 200);

    let r = engine.run(script_request("/gtm.js", "id=GTM-ABC"));
    assert_eq!(r.claimed_by.as_deref(), Some("Google scripts"));
    assert_eq!(r.response.status, 200, "{:?}", r.logs);
    assert_eq!(r.response.body, b"ok");
    assert!(r.response.headers.contains(&(
        "content-type".into(),
        "application/javascript; charset=utf-8".into()
    )));
    assert_eq!(
        rec.sent.lock().unwrap()[0].url,
        "https://www.googletagmanager.com/gtm.js?id=GTM-ABC"
    );

    // Served from the cache: no second fetch.
    let r = engine.run(script_request("/gtm.js", "id=GTM-ABC"));
    assert_eq!(r.response.status, 200);
    assert_eq!(rec.sent.lock().unwrap().len(), 1);

    let r = engine.run(script_request("/gtag/js", "id=G-XYZ&cx=c"));
    assert_eq!(r.response.status, 200);
    assert_eq!(
        rec.sent.lock().unwrap()[1].url,
        "https://www.googletagmanager.com/gtag/js?cx=c&id=G-XYZ"
    );

    let r = engine.run(script_request("/gtag/destination", "id=G-XYZ&cx=c"));
    assert_eq!(r.response.status, 200);
    assert_eq!(
        rec.sent.lock().unwrap()[2].url,
        "https://www.googletagmanager.com/gtag/destination?cx=c&id=G-XYZ"
    );
}

#[test]
fn points_gtm_script_host_at_this_server() {
    let gtm = r#"var c={"21":"www.googletagmanager.com","23":"q","3":"www.googletagmanager.com"};"#;
    let c = Container::builtin_with_scripts(&["GTM-ABC".into()]);
    let (engine, _) = engine_answering(c, 200, gtm);

    let r = engine.run(script_request("/gtm.js", "id=GTM-ABC"));
    assert_eq!(
        String::from_utf8_lossy(&r.response.body),
        r#"var c={"21":"www.googletagmanager.com","23":"q","3":"example.com"};"#
    );

    // A proxy on another path passes its prefix, and the cached copy is
    // rewritten for each request.
    let mut req = script_request("/gtm.js", "id=GTM-ABC");
    req.headers.push(("x-forwarded-prefix".into(), "/t".into()));
    let r = engine.run(req);
    assert!(String::from_utf8_lossy(&r.response.body).contains(r#""3":"example.com/t""#));
}

#[test]
fn serves_gtm_service_worker_files_only() {
    let (engine, rec) = engine_with(Container::builtin_with_scripts(&["GTM-ABC".into()]), 200);

    let r = engine.run(script_request(
        "/_/service_worker/6a60/sw_iframe.html",
        "origin=https%3A%2F%2Fexample.com&1p=1&path=%2Fmetrics",
    ));
    assert_eq!(r.response.status, 200);
    assert!(
        r.response
            .headers
            .contains(&("content-type".into(), "text/html; charset=utf-8".into()))
    );
    assert!(r.response.headers.contains(&(
        "content-security-policy".into(),
        "frame-ancestors 'self'".into()
    )));
    let r = engine.run(script_request("/_/service_worker/6a60/sw.js", ""));
    assert_eq!(r.response.status, 200);
    assert_eq!(
        rec.sent
            .lock()
            .unwrap()
            .iter()
            .map(|o| o.url.as_str())
            .collect::<Vec<_>>(),
        [
            "https://www.googletagmanager.com/static/service_worker/6a60/sw_iframe.html",
            "https://www.googletagmanager.com/static/service_worker/6a60/sw.js",
        ]
    );

    for path in [
        "/_/service_worker/6a60/other.js",
        "/_/service_worker/../gtm.js",
        "/_/service_worker/6a60/sw.js/x",
    ] {
        let r = engine.run(script_request(path, ""));
        assert!(r.claimed_by.is_none(), "{path}");
    }
    assert_eq!(rec.sent.lock().unwrap().len(), 2);
}

#[test]
fn refuses_scripts_for_other_ids() {
    let (engine, rec) = engine_with(Container::builtin_with_scripts(&["GTM-ABC".into()]), 200);
    let r = engine.run(script_request("/gtm.js", "id=GTM-OTHER"));
    assert!(r.claimed_by.is_none());
    assert_eq!(r.response.status, 400);
    assert!(rec.sent.lock().unwrap().is_empty());
}

#[test]
fn script_upstream_error_is_passed_on() {
    let (engine, _) = engine_with(Container::builtin_with_scripts(&["GTM-ABC".into()]), 404);
    let r = engine.run(script_request("/gtm.js", "id=GTM-ABC"));
    assert_eq!(r.response.status, 404);
}

fn protected(origin_check: &str) -> Container {
    Container::builtin_with(&Settings {
        measurement_ids: vec!["G-TEST000001".into()],
        origin_check: origin_check.into(),
        ..Default::default()
    })
}

fn with_header(mut req: IncomingRequest, name: &str, value: Option<&str>) -> IncomingRequest {
    req.headers.retain(|(k, _)| k != name);
    if let Some(v) = value {
        req.headers.push((name.into(), v.into()));
    }
    req
}

#[test]
fn hits_for_other_properties_are_not_claimed() {
    let (engine, rec) = engine_with(protected("log"), 204);
    let r = engine.run(ga4_hit("v=2&tid=G-NOTYOURS1&cid=1.2&en=page_view", ""));
    assert!(r.claimed_by.is_none());
    assert!(rec.sent.lock().unwrap().is_empty());

    // A batched line can't switch to another property either.
    let r = engine.run(ga4_hit(
        "v=2&tid=G-TEST000001&cid=1.2",
        "en=page_view\r\nen=scroll&tid=G-NOTYOURS1",
    ));
    assert_eq!(r.response.status, 204);
    let sent = rec.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].url.contains("en=page_view"), "{}", sent[0].url);
}

#[test]
fn foreign_origin_is_logged_or_refused() {
    let hit = || {
        with_header(
            ga4_hit("v=2&tid=G-TEST000001&cid=1.2&en=page_view", ""),
            "origin",
            Some("https://evil.example"),
        )
    };

    let (engine, rec) = engine_with(protected("log"), 204);
    let r = engine.run(hit());
    assert_eq!(r.response.status, 204);
    assert_eq!(rec.sent.lock().unwrap().len(), 1);
    assert!(
        r.logs
            .iter()
            .any(|l| l.contains("origin_check=would_reject origin=evil.example")),
        "{:?}",
        r.logs
    );
    assert!(
        !r.response
            .headers
            .iter()
            .any(|(k, _)| k == "access-control-allow-origin")
    );

    let (engine, rec) = engine_with(protected("enforce"), 204);
    let r = engine.run(hit());
    assert_eq!(r.response.status, 403);
    assert!(rec.sent.lock().unwrap().is_empty());

    // The site's own Referer is enough when there's no Origin.
    let r = engine.run(with_header(
        with_header(hit(), "origin", None),
        "referer",
        Some("https://example.com/blog/x"),
    ));
    assert_eq!(r.response.status, 204);
    assert_eq!(rec.sent.lock().unwrap().len(), 1);
}

#[test]
fn bot_user_agents_are_not_forwarded() {
    let (engine, rec) = engine_with(protected("enforce"), 204);
    for ua in [
        Some("Mozilla/5.0 (compatible; Googlebot/2.1)"),
        Some("AdsBot-Google (+http://www.google.com/adsbot.html)"),
        Some("Mozilla/5.0 (compatible; PetalBot;+https://webmaster.petalsearch.com)"),
        Some("HeadlessChrome/120"),
        Some("curl/8.5.0"),
        Some("python-requests/2.32"),
        Some("Go-http-client/2.0"),
        None,
    ] {
        let r = engine.run(with_header(
            ga4_hit("v=2&tid=G-TEST000001&cid=1.2&en=page_view", ""),
            "user-agent",
            ua,
        ));
        assert_eq!(r.response.status, 204, "{ua:?}");
    }
    assert!(rec.sent.lock().unwrap().is_empty());
}

#[test]
fn unknown_script_parameters_do_not_bypass_the_cache() {
    let (engine, rec) = engine_with(Container::builtin_with_scripts(&["GTM-ABC".into()]), 200);
    for q in ["id=GTM-ABC&bust=1", "bust=2&id=GTM-ABC", "id=GTM-ABC"] {
        assert_eq!(
            engine.run(script_request("/gtm.js", q)).response.status,
            200
        );
    }
    let sent = rec.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].url,
        "https://www.googletagmanager.com/gtm.js?id=GTM-ABC"
    );
}

#[test]
fn phones_named_like_bots_are_still_forwarded() {
    let (engine, rec) = engine_with(protected("enforce"), 204);
    for ua in [
        "Mozilla/5.0 (Linux; Android 10; Cubot P40) AppleWebKit/537.36 Chrome/120 Mobile",
        "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) Safari/604.1",
    ] {
        let r = engine.run(with_header(
            ga4_hit("v=2&tid=G-TEST000001&cid=1.2&en=page_view", ""),
            "user-agent",
            Some(ua),
        ));
        assert_eq!(r.response.status, 204);
    }
    assert_eq!(rec.sent.lock().unwrap().len(), 2);
}

#[test]
fn site_domains_decide_which_origins_count() {
    let c = Container::builtin_with(&Settings {
        measurement_ids: vec!["G-TEST000001".into()],
        site_domains: vec!["example.com".into()],
        origin_check: "enforce".into(),
        ..Default::default()
    });
    let (engine, rec) = engine_with(c, 204);
    let hit = |origin: &str| {
        let req = with_header(
            ga4_hit("v=2&tid=G-TEST000001&cid=1.2&en=page_view", ""),
            "host",
            Some("metrics.example.com"),
        );
        with_header(req, "origin", Some(origin))
    };
    for (origin, status) in [
        ("https://example.com", 204),
        ("https://www.example.com", 204),
        ("https://evilexample.com", 403),
        ("https://example.com.evil.example", 403),
    ] {
        let r = engine.run(hit(origin));
        assert_eq!(r.response.status, status, "{origin}");
        assert!(
            r.response
                .headers
                .contains(&("vary".into(), "origin".into()))
        );
    }
    assert_eq!(rec.sent.lock().unwrap().len(), 2);
}

#[test]
fn script_parameter_values_must_be_plain_tokens() {
    let (engine, rec) = engine_with(Container::builtin_with_scripts(&["G-XYZ".into()]), 200);
    for q in [
        "id=G-XYZ&l=dataLayer",
        "l=a%2Fb&id=G-XYZ",
        "id=G-XYZ&cx=<x>",
    ] {
        engine.run(script_request("/gtag/js", q));
    }
    let sent = rec.sent.lock().unwrap();
    let urls: Vec<&str> = sent.iter().map(|o| o.url.as_str()).collect();
    assert_eq!(
        urls,
        [
            "https://www.googletagmanager.com/gtag/js?id=G-XYZ&l=dataLayer",
            "https://www.googletagmanager.com/gtag/js?id=G-XYZ",
        ]
    );
}

/// Builtin client plus one custom tag holding exactly `perms`.
fn permitted_tag(src: &str, perms: serde_json::Value) -> Container {
    let mut c = custom(&[("t", src)], &["ga4_client"], &["t"]);
    c.permissions
        .insert("t".into(), crate::permissions::from_json(&perms).unwrap());
    c
}

const HIT: &str = "v=2&tid=G-1&cid=1.2&en=purchase&ep.secret=s3";

#[test]
fn permitted_urls_are_sent_and_others_are_refused() {
    let src = "const send = require('sendHttpRequest');\n\
        send(data.url).then(data.gtmOnSuccess, data.gtmOnFailure);";
    for (url, sent, outcome) in [
        ("https://example.com/ok", 1, "success"),
        ("https://evil.test/x", 0, "failure"),
    ] {
        let mut c = permitted_tag(
            src,
            json!({"send_http": {"urls": ["https://example.com/*"]}}),
        );
        c.tags[0].data = json!({ "url": url });
        let (engine, rec) = engine_with(c, 200);
        let r = engine.run(ga4_hit(HIT, ""));
        assert_eq!(rec.sent.lock().unwrap().len(), sent, "{url}: {:?}", r.logs);
        assert_eq!(r.tags[0].outcome, outcome, "{url}: {:?}", r.tags);
    }
}

#[test]
fn event_data_is_limited_to_the_permitted_keys() {
    let src = "const get = require('getEventData');\n\
        data.seen = [get('event_name'), get('x-ga-mp2-ep.secret')];\n\
        require('logToConsole')(JSON.stringify(data.seen));\n\
        data.gtmOnSuccess();";
    let src = src.replace("JSON.stringify(data.seen)", "String(data.seen)");
    let c = permitted_tag(
        &src,
        json!({"read_event_data": {"keys": ["event_name"]}, "logging": {}}),
    );
    let (engine, _) = engine_with(c, 200);
    let r = engine.run(ga4_hit(HIT, ""));
    // The unpermitted key throws before logging.
    assert_eq!(r.tags[0].outcome, "failure", "{:?}", r.logs);
    assert!(
        r.tags[0]
            .error
            .as_deref()
            .unwrap_or("")
            .contains("not allowed"),
        "{:?}",
        r.tags
    );
}

#[test]
fn getting_all_event_data_needs_any_key() {
    let src = "require('getAllEventData')();\ndata.gtmOnSuccess();";
    let narrow = permitted_tag(src, json!({"read_event_data": {"keys": ["event_name"]}}));
    let (engine, _) = engine_with(narrow, 200);
    assert_eq!(engine.run(ga4_hit(HIT, "")).tags[0].outcome, "failure");

    let any = permitted_tag(src, json!({"read_event_data": {"keys": ["*"]}}));
    let (engine, _) = engine_with(any, 200);
    assert_eq!(engine.run(ga4_hit(HIT, "")).tags[0].outcome, "success");
}

#[test]
fn an_api_without_its_permission_cannot_be_required() {
    let src = "require('logToConsole')('hi');\ndata.gtmOnSuccess();";
    let c = permitted_tag(src, json!({}));
    let (engine, _) = engine_with(c, 200);
    let r = engine.run(ga4_hit(HIT, ""));
    assert_eq!(r.tags[0].outcome, "failure");
    assert!(
        r.logs.iter().any(|l| l.contains("\"logging\" permission")),
        "{:?}",
        r.logs
    );
}

#[test]
fn builtin_templates_pass_the_load_time_check() {
    for (name, src) in Container::builtin().templates {
        crate::permissions::check_template(&src, None).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}

#[test]
fn every_listed_api_can_be_required_by_a_trusted_template() {
    let table: std::collections::HashMap<String, Option<String>> =
        serde_json::from_str(crate::permissions::API_PERMISSIONS).unwrap();
    let src: String = table
        .keys()
        .map(|k| format!("require('{k}');\n"))
        .collect::<String>()
        + "data.gtmOnSuccess();";
    let c = custom(&[("t", &src)], &["ga4_client"], &["t"]);
    let (engine, _) = engine_with(c, 200);
    let r = engine.run(ga4_hit(HIT, ""));
    assert_eq!(r.tags[0].outcome, "success", "{:?}", r.logs);
}

const RUN_ONE_EVENT: &str = r#"
    require('claimRequest')();
    require('runContainer')({event_name: 'x'}, () => require('returnResponse')());
"#;

#[test]
fn hmac_sha256_signs_in_base64_or_hex() {
    let tag = r#"
        const hmac = require('hmacSha256');
        const log = require('logToConsole');
        hmac('what do ya want for nothing?', 'Jefe', (sig) => {
            log(sig);
            hmac('what do ya want for nothing?', 'Jefe', (hex) => {
                log(hex);
                data.gtmOnSuccess();
            }, data.gtmOnFailure, { outputEncoding: 'hex' });
        }, data.gtmOnFailure);
    "#;
    let (engine, _) = engine_with(
        custom(&[("c", RUN_ONE_EVENT), ("t", tag)], &["c"], &["t"]),
        204,
    );
    let r = engine.run(ga4_hit("", ""));
    assert_eq!(r.tags[0].outcome, "success", "{:?}", r.logs);
    // RFC 4231 test case 2.
    let hex = "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843";
    assert!(r.logs.contains(&format!("[t] {hex}")), "{:?}", r.logs);
    assert!(
        r.logs
            .iter()
            .any(|l| l.ends_with("W9zBRr9gdU5qBCQmCJV1x1oAPwidJzmDnexYuWTsOEM=")),
        "{:?}",
        r.logs
    );
}

#[test]
fn legacy_hits_are_detected_and_mapped() {
    let client = r#"
        const log = require('logToConsole');
        if (require('isRequestMpv1')()) {
            require('claimRequest')();
            const events = require('extractEventsFromMpv1')();
            log(events[0].event_name + ' ' + events[0].client_id);
            require('runContainer')(events[0], () => require('returnResponse')());
        }
    "#;
    let (engine, _) = engine_with(custom(&[("c", client)], &["c"], &[]), 204);
    let mut req = ga4_hit("v=1&t=pageview&cid=42&dp=%2Fa", "");
    req.path = "/collect".into();
    let r = engine.run(req);
    assert_eq!(r.claimed_by.as_deref(), Some("c"));
    assert!(
        r.logs.contains(&"[c] page_view 42".to_string()),
        "{:?}",
        r.logs
    );
    // A GA4 hit is not a legacy one.
    assert_eq!(engine.run(ga4_hit("v=2&en=x", "")).claimed_by, None);
}

#[test]
fn event_callback_runs_after_all_tags_with_their_outcomes() {
    let waits = r#"
        require('addEventCallback')((containerId, eventData) => {
            require('logToConsole')(JSON.stringify(eventData.tags.map(t => t.id + ':' + t.status)));
        });
        data.gtmOnSuccess();
    "#;
    let fails = "data.gtmOnFailure();";
    let (engine, _) = engine_with(
        custom(
            &[("c", RUN_ONE_EVENT), ("a", waits), ("b", fails)],
            &["c"],
            &["a", "b"],
        ),
        204,
    );
    let r = engine.run(ga4_hit("", ""));
    assert_eq!(r.response.status, 200);
    assert!(
        r.logs
            .contains(&r#"[a] ["a:success","b:failure"]"#.to_string()),
        "{:?}",
        r.logs
    );
}

#[test]
fn templates_exchange_messages_by_type() {
    let listener = r#"
        require('addMessageListener')('greeting', (type, message) => {
            require('logToConsole')('got ' + type + ' ' + message.name);
        });
        data.gtmOnSuccess();
    "#;
    let sender = r#"
        const has = require('hasMessageListener');
        require('logToConsole')('listener? ' + has('greeting') + ' / ' + has('other'));
        require('sendMessage')('greeting', { name: 'ada' });
        require('sendMessage')('other', { name: 'nobody' });
        data.gtmOnSuccess();
    "#;
    let (engine, _) = engine_with(
        custom(
            &[("c", RUN_ONE_EVENT), ("l", listener), ("s", sender)],
            &["c"],
            &["l", "s"],
        ),
        204,
    );
    let r = engine.run(ga4_hit("", ""));
    assert!(
        r.logs.contains(&"[s] listener? true / false".to_string()),
        "{:?}",
        r.logs
    );
    assert!(
        r.logs.contains(&"[l] got greeting ada".to_string()),
        "{:?}",
        r.logs
    );
    assert!(!r.logs.iter().any(|l| l.contains("nobody")), "{:?}", r.logs);
}

#[test]
fn templates_that_answer_each_other_stop_at_the_message_cap() {
    let a = r#"
        const send = require('sendMessage');
        require('addMessageListener')('ping', () => send('pong', 1));
        send('ping', 1);
        data.gtmOnSuccess();
    "#;
    let b = r#"
        const send = require('sendMessage');
        require('addMessageListener')('pong', () => send('ping', 1));
        data.gtmOnSuccess();
    "#;
    let (engine, _) = engine_with(
        custom(
            &[("c", RUN_ONE_EVENT), ("a", a), ("b", b)],
            &["c"],
            &["a", "b"],
        ),
        204,
    );
    let r = engine.run(ga4_hit("", ""));
    assert_eq!(r.response.status, 200, "{:?}", r.logs);
}

/// A container with the given ad platforms switched on, built the way the
/// admin page does it.
fn providers(setup: &[(&str, &[(&str, &str)])], enrich: Option<&str>) -> Container {
    use crate::integrations::{Enrichment, Integrations};
    use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
    static N: AtomicU32 = AtomicU32::new(0);
    let path = std::env::temp_dir().join(format!(
        "sandbox-int-{}-{}.json",
        std::process::id(),
        N.fetch_add(1, Relaxed)
    ));
    let base = crate::container_file::ContainerFile::fixed(
        Container::builtin(),
        Default::default(),
        Default::default(),
    );
    let i = Integrations::open(&path, base).unwrap();
    for (id, fields) in setup {
        let f = fields
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        i.set_provider(id, true, f).unwrap();
    }
    if let Some(url) = enrich {
        i.set_enrichment(Enrichment {
            enabled: true,
            url: url.into(),
            header_name: "x-api-key".into(),
            header_value: "k".into(),
            ..Default::default()
        })
        .unwrap();
    }
    std::fs::remove_file(&path).ok();
    (*i.resolved().container).clone()
}

const META: (&str, &[(&str, &str)]) = (
    "meta",
    &[
        ("pixel_id", "123"),
        ("access_token", "tok&en"),
        ("test_event_code", "TEST1"),
    ],
);
const TIKTOK: (&str, &[(&str, &str)]) = (
    "tiktok",
    &[("pixel_code", "P1"), ("access_token", "tt-tok")],
);
const PINTEREST: (&str, &[(&str, &str)]) = (
    "pinterest",
    &[
        ("ad_account_id", "999"),
        ("access_token", "pin-tok"),
        ("test_mode", "true"),
    ],
);

const SNAPCHAT: (&str, &[(&str, &str)]) = (
    "snapchat",
    &[
        ("pixel_id", "snap-px"),
        ("access_token", "snap-tok"),
        ("test_mode", "true"),
    ],
);
const REDDIT: (&str, &[(&str, &str)]) = (
    "reddit",
    &[
        ("pixel_id", "a2_px"),
        ("access_token", "rdt-tok"),
        ("test_event_code", "t-1"),
    ],
);
const LINKEDIN: (&str, &[(&str, &str)]) = (
    "linkedin",
    &[
        ("access_token", "li-tok"),
        ("conversions", "purchase=111, generate_lead=222"),
    ],
);
const X: (&str, &[(&str, &str)]) = (
    "x",
    &[
        ("pixel_id", "o8z1j"),
        ("event_ids", "purchase=tw-o8z1j-o8z2m"),
        ("consumer_key", "ck"),
        ("consumer_secret", "cs"),
        ("access_token", "at"),
        ("access_token_secret", "ats"),
    ],
);

fn meta_hit(query: &str) -> IncomingRequest {
    let mut req = ga4_hit(query, "");
    req.headers
        .push(("cookie".into(), "_fbp=fb.1.1.2; _fbc=fb.1.3.abc".into()));
    req
}

// Consent Mode v2 as a banner sends it after "accept all": every signal denied
// by default, then granted ("r").
const PURCHASE: &str = "v=2&tid=G-1&cid=1.2&en=purchase&uid=User%40Example.com&cu=EUR&epn.value=19.9&ep.transaction_id=T1&ep.event_id=ev-1&dl=https%3A%2F%2Fexample.com%2Fthanks&gcs=G111&gcd=13r3r3r2r5l1";

/// Sends of one run that went to `host`, as parsed JSON bodies.
fn bodies(rec: &Recorder, host: &str) -> Vec<(Outgoing, serde_json::Value)> {
    rec.sent
        .lock()
        .unwrap()
        .iter()
        .filter(|o| o.url.starts_with(host))
        .map(|o| (o.clone(), serde_json::from_str(&o.body).unwrap_or_default()))
        .collect()
}

// sha256("user@example.com"): trimmed and lower-cased before hashing.
const EMAIL_HASH: &str = "b4c9a289323b21a01c3e940f150eb9b8c542587f1abfd8f0e1cc1ffc5e475514";

#[test]
fn meta_sends_a_purchase_with_hashed_user_data() {
    let (engine, rec) = engine_with(providers(&[META], None), 200);
    let r = engine.run(meta_hit(PURCHASE));
    assert_eq!(r.tags[1].outcome, "success", "{:?} {:?}", r.tags, r.logs);
    let sent = bodies(&rec, "https://graph.facebook.com");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0.url, "https://graph.facebook.com/v26.0/123/events");
    let body = &sent[0].1;
    assert_eq!(body["access_token"], "tok&en");
    assert_eq!(body["test_event_code"], "TEST1");
    assert_eq!(body["partner_agent"], "analitikisler-tagging-server");
    let ev = &body["data"][0];
    assert_eq!(ev["event_name"], "Purchase");
    assert_eq!(ev["event_id"], "ev-1");
    assert_eq!(ev["action_source"], "website");
    assert_eq!(ev["event_source_url"], "https://example.com/thanks");
    assert_eq!(ev["custom_data"]["value"], 19.9);
    assert_eq!(ev["custom_data"]["currency"], "EUR");
    assert_eq!(ev["custom_data"]["order_id"], "T1");
    assert_eq!(ev["user_data"]["client_ip_address"], "203.0.113.9");
    assert_eq!(ev["user_data"]["fbp"], "fb.1.1.2");
    assert_eq!(ev["user_data"]["fbc"], "fb.1.3.abc");
    assert_eq!(ev["user_data"]["external_id"][0], EMAIL_HASH);
}

#[test]
fn tiktok_sends_a_payment_with_its_own_shape() {
    let (engine, rec) = engine_with(providers(&[TIKTOK], None), 200);
    let mut req = ga4_hit(PURCHASE, "");
    req.headers.push(("cookie".into(), "_ttp=ttp-1".into()));
    let r = engine.run(req);
    assert_eq!(r.tags[1].outcome, "success", "{:?} {:?}", r.tags, r.logs);
    let sent = bodies(&rec, "https://business-api.tiktok.com");
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].0.url,
        "https://business-api.tiktok.com/open_api/v1.3/event/track/"
    );
    assert!(
        sent[0]
            .0
            .headers
            .contains(&("access-token".into(), "tt-tok".into()))
    );
    let body = &sent[0].1;
    assert_eq!(body["event_source"], "web");
    assert_eq!(body["event_source_id"], "P1");
    let ev = &body["data"][0];
    assert_eq!(ev["event"], "CompletePayment");
    assert_eq!(ev["event_id"], "ev-1");
    assert_eq!(ev["user"]["ip"], "203.0.113.9");
    assert_eq!(ev["user"]["external_id"], EMAIL_HASH);
    assert_eq!(ev["page"]["url"], "https://example.com/thanks");
    assert_eq!(ev["properties"]["value"], 19.9);
    assert_eq!(ev["properties"]["currency"], "EUR");
}

#[test]
fn pinterest_sends_a_checkout_to_the_test_endpoint() {
    let (engine, rec) = engine_with(providers(&[PINTEREST], None), 200);
    let r = engine.run(ga4_hit(PURCHASE, ""));
    assert_eq!(r.tags[1].outcome, "success", "{:?} {:?}", r.tags, r.logs);
    let sent = bodies(&rec, "https://api.pinterest.com");
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].0.url,
        "https://api.pinterest.com/v5/ad_accounts/999/events?test=true"
    );
    assert!(
        sent[0]
            .0
            .headers
            .contains(&("authorization".into(), "Bearer pin-tok".into()))
    );
    let ev = &sent[0].1["data"][0];
    assert_eq!(ev["event_name"], "checkout");
    assert_eq!(ev["action_source"], "web");
    assert_eq!(ev["custom_data"]["value"], "19.9");
    assert_eq!(ev["user_data"]["external_id"][0], EMAIL_HASH);
}

#[test]
fn snapchat_sends_a_purchase_to_the_validate_endpoint() {
    let hit = format!("{PURCHASE}&pr1=idSKU1~pr19.9~qt2");
    let mut req = ga4_hit(&hit, "");
    req.headers.push(("cookie".into(), "_scid=sc-1".into()));
    let (engine, rec) = engine_with(providers(&[SNAPCHAT], None), 200);
    let r = engine.run(req);
    assert_eq!(r.tags[1].outcome, "success", "{:?} {:?}", r.tags, r.logs);
    let sent = bodies(&rec, "https://tr.snapchat.com");
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].0.url,
        "https://tr.snapchat.com/v3/snap-px/events/validate?access_token=snap-tok"
    );
    let ev = &sent[0].1["data"][0];
    assert_eq!(ev["event_name"], "PURCHASE");
    assert_eq!(ev["action_source"], "WEB");
    assert_eq!(ev["event_id"], "ev-1");
    assert_eq!(ev["user_data"]["sc_cookie1"], "sc-1");
    assert_eq!(ev["user_data"]["external_id"][0], EMAIL_HASH);
    assert_eq!(ev["custom_data"]["value"], 19.9);
    assert_eq!(ev["custom_data"]["currency"], "EUR");
    assert_eq!(
        ev["custom_data"]["contents"],
        serde_json::json!([{"id": "SKU1", "quantity": 2, "item_price": 19.9}])
    );
}

#[test]
fn reddit_sends_a_purchase_with_its_test_id() {
    let hit = format!("{PURCHASE}&pr1=idSKU1~nmShirt~pr19.9~qt2");
    let mut req = ga4_hit(&hit, "");
    req.headers
        .push(("cookie".into(), "_rdt_uuid=u-1; _rdt_cid=c-1".into()));
    let (engine, rec) = engine_with(providers(&[REDDIT], None), 200);
    let r = engine.run(req);
    assert_eq!(r.tags[1].outcome, "success", "{:?} {:?}", r.tags, r.logs);
    let sent = bodies(&rec, "https://ads-api.reddit.com");
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].0.url,
        "https://ads-api.reddit.com/api/v3/pixels/a2_px/conversion_events"
    );
    assert!(
        sent[0]
            .0
            .headers
            .contains(&("authorization".into(), "Bearer rdt-tok".into()))
    );
    let body = &sent[0].1["data"];
    assert_eq!(body["test_id"], "t-1");
    let ev = &body["events"][0];
    assert_eq!(ev["type"]["tracking_type"], "PURCHASE");
    assert_eq!(ev["action_source"], "WEBSITE");
    assert!(ev["event_at"].as_u64().unwrap() > 1_600_000_000_000);
    assert_eq!(ev["click_id"], "c-1");
    assert_eq!(ev["user"]["uuid"], "u-1");
    assert_eq!(ev["user"]["external_id"], EMAIL_HASH);
    assert_eq!(ev["metadata"]["conversion_id"], "ev-1");
    assert_eq!(ev["metadata"]["value"], 19.9);
    assert_eq!(ev["metadata"]["item_count"], 2);
    assert_eq!(
        ev["metadata"]["products"],
        serde_json::json!([{"id": "SKU1", "name": "Shirt"}])
    );
}

#[test]
fn linkedin_sends_mapped_conversions_and_skips_unmatchable_visitors() {
    // No email and no li_fat_id: LinkedIn can't match the visitor.
    let (engine, rec) = engine_with(providers(&[LINKEDIN], None), 201);
    let r = engine.run(ga4_hit(PURCHASE, ""));
    assert_eq!(
        r.tags[1].skip.as_deref(),
        Some("no_user_id"),
        "{:?}",
        r.tags
    );
    assert!(bodies(&rec, "https://api.linkedin.com").is_empty());

    let hit = PURCHASE.replace("thanks", "thanks%3Fli_fat_id%3Dfat-1");
    let r = engine.run(ga4_hit(&hit, ""));
    assert_eq!(r.tags[1].outcome, "success", "{:?} {:?}", r.tags, r.logs);
    let sent = bodies(&rec, "https://api.linkedin.com");
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].0.url,
        "https://api.linkedin.com/rest/conversionEvents"
    );
    for h in [
        ("authorization", "Bearer li-tok"),
        ("x-restli-protocol-version", "2.0.0"),
    ] {
        assert!(
            sent[0].0.headers.contains(&(h.0.into(), h.1.into())),
            "{h:?}"
        );
    }
    let ev = &sent[0].1;
    assert_eq!(ev["conversion"], "urn:lla:llaPartnerConversion:111");
    assert_eq!(ev["eventId"], "ev-1");
    assert_eq!(
        ev["conversionValue"],
        serde_json::json!({"currencyCode": "EUR", "amount": "19.9"})
    );
    assert_eq!(
        ev["user"]["userIds"],
        serde_json::json!([{"idType": "LINKEDIN_FIRST_PARTY_ADS_TRACKING_UUID", "idValue": "fat-1"}])
    );

    // An event with no conversion rule is skipped.
    let r = engine.run(ga4_hit(&hit.replace("en=purchase", "en=add_to_cart"), ""));
    assert_eq!(r.tags[1].skip.as_deref(), Some("unmapped_event"));
}

#[test]
fn x_sends_a_signed_conversion() {
    let (engine, rec) = engine_with(providers(&[X], None), 200);
    let r = engine.run(ga4_hit(PURCHASE, ""));
    assert_eq!(
        r.tags[1].skip.as_deref(),
        Some("no_user_id"),
        "{:?}",
        r.tags
    );

    let hit = format!(
        "{}&pr1=idSKU1~nmShirt~pr19.9~qt2",
        PURCHASE.replace("thanks", "thanks%3Ftwclid%3Dtw-click")
    );
    let r = engine.run(ga4_hit(&hit, ""));
    assert_eq!(r.tags[1].outcome, "success", "{:?} {:?}", r.tags, r.logs);
    let sent = bodies(&rec, "https://ads-api.twitter.com");
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].0.url,
        "https://ads-api.twitter.com/12/measurement/conversions/o8z1j"
    );
    let auth = &sent[0]
        .0
        .headers
        .iter()
        .find(|(k, _)| k == "authorization")
        .unwrap()
        .1;
    assert!(
        auth.starts_with("OAuth oauth_consumer_key=\"ck\", oauth_nonce=\""),
        "{auth}"
    );
    assert!(
        auth.contains("oauth_signature_method=\"HMAC-SHA1\""),
        "{auth}"
    );
    assert!(auth.contains("oauth_token=\"at\""), "{auth}");
    let c = &sent[0].1["conversions"][0];
    assert_eq!(c["event_id"], "tw-o8z1j-o8z2m");
    assert_eq!(c["conversion_id"], "ev-1");
    let time = c["conversion_time"].as_str().unwrap();
    assert!(time.len() == 24 && time.ends_with('Z'), "{time}");
    assert_eq!(
        c["identifiers"][0],
        serde_json::json!({"twclid": "tw-click"})
    );
    assert_eq!(
        c["identifiers"][1],
        serde_json::json!({"ip_address": "203.0.113.9", "user_agent": "Mozilla/5.0 Test"})
    );
    assert_eq!(c["price_currency"], "EUR");
    assert_eq!(c["number_items"], 2);
    assert_eq!(
        c["contents"],
        serde_json::json!([{"content_id": "SKU1", "content_name": "Shirt", "content_price": 19.9, "num_items": 2}])
    );
}

#[test]
fn purchase_items_reach_every_platform() {
    let hit =
        format!("{PURCHASE}&pr1=idSKU1~nmShirt~brAcme~caApparel~pr19.9~qt2&pr2=idSKU2~nmMug~pr5");
    let (engine, rec) = engine_with(providers(&[META, TIKTOK, PINTEREST], None), 200);
    let r = engine.run(meta_hit(&hit));
    assert!(
        r.tags[1..].iter().all(|t| t.outcome == "success"),
        "{:?} {:?}",
        r.tags,
        r.logs
    );

    let meta = &bodies(&rec, "https://graph.facebook.com")[0].1["data"][0]["custom_data"];
    assert_eq!(meta["content_type"], "product");
    assert_eq!(meta["content_ids"], serde_json::json!(["SKU1", "SKU2"]));
    assert_eq!(
        meta["contents"],
        serde_json::json!([
            {"id": "SKU1", "quantity": 2, "item_price": 19.9},
            {"id": "SKU2", "quantity": 1, "item_price": 5}
        ])
    );
    assert_eq!(meta["num_items"], 3);

    let tt = &bodies(&rec, "https://business-api.tiktok.com")[0].1["data"][0]["properties"];
    assert_eq!(tt["content_type"], "product");
    assert_eq!(
        tt["contents"][0],
        serde_json::json!({
            "content_id": "SKU1", "content_name": "Shirt", "content_category": "Apparel",
            "brand": "Acme", "price": 19.9, "quantity": 2
        })
    );
    assert_eq!(tt["contents"][1]["quantity"], 1);

    let pin = &bodies(&rec, "https://api.pinterest.com")[0].1["data"][0]["custom_data"];
    assert_eq!(pin["content_ids"], serde_json::json!(["SKU1", "SKU2"]));
    assert_eq!(pin["num_items"], 3);
    assert_eq!(
        pin["contents"][0],
        serde_json::json!({
            "id": "SKU1", "item_name": "Shirt", "item_category": "Apparel",
            "item_brand": "Acme", "item_price": "19.9", "quantity": 2
        })
    );
}

#[test]
fn events_without_items_send_no_item_fields() {
    let (engine, rec) = engine_with(providers(&[META], None), 200);
    engine.run(meta_hit(PURCHASE));
    let meta = &bodies(&rec, "https://graph.facebook.com")[0].1["data"][0]["custom_data"];
    for key in ["content_type", "content_ids", "contents", "num_items"] {
        assert!(meta.get(key).is_none(), "{key}");
    }
}

#[test]
fn ad_tags_skip_with_a_reason_and_send_nothing() {
    const BASE: &str = "v=2&tid=G-1&cid=1.2&en=purchase";
    for (q, reason) in [
        (
            "v=2&tid=G-1&cid=1.2&en=scroll&gcd=13r3r3r2r5l1".to_string(),
            "unmapped_event",
        ),
        // Today's banner: analytics only, so gcs=G101.
        (format!("{BASE}&gcs=G101"), "consent:ad_storage=denied"),
        // Ad storage granted, ad user data denied after the banner ("q").
        (
            format!("{BASE}&gcs=G111&gcd=13r3r3q2r5l1"),
            "consent:ad_user_data=denied",
        ),
        // gcd wins over gcs; ad storage denied by default only ("p").
        (
            format!("{BASE}&gcs=G111&gcd=13p3r3r2r5l1"),
            "consent:ad_storage=denied",
        ),
        // gcs alone can't say anything about ad user data.
        (format!("{BASE}&gcs=G111"), "consent:ad_user_data=missing"),
        (BASE.to_string(), "consent:ad_storage=missing"),
        (
            format!("{BASE}&gcd=13l3r3r2r5l1"),
            "consent:ad_storage=missing",
        ),
    ] {
        let (engine, rec) = engine_with(
            providers(
                &[META, TIKTOK, PINTEREST, SNAPCHAT, REDDIT, LINKEDIN, X],
                None,
            ),
            200,
        );
        let r = engine.run(meta_hit(&q));
        assert_eq!(r.response.status, 204, "{q}");
        assert_eq!(r.tags.len(), 8, "{q}");
        for t in &r.tags[1..] {
            assert_eq!(t.outcome, "skipped", "{q}: {:?}", r.tags);
            assert_eq!(t.skip.as_deref(), Some(reason), "{q}");
        }
        let sent = rec.sent.lock().unwrap();
        assert!(
            sent.iter().all(|o| o.url.contains("google-analytics")),
            "{q}: {:?}",
            sent.iter().map(|o| &o.url).collect::<Vec<_>>()
        );
    }
}

#[test]
fn consent_granted_by_default_or_by_update_sends() {
    // "t": granted by default, "v": granted by default and after the banner,
    // "n": granted by the banner with no default.
    for gcd in ["11t1t1t1t5", "13v3v3v2v5", "13n3n3n2n5"] {
        let (engine, rec) = engine_with(providers(&[META], None), 200);
        let q = format!("v=2&tid=G-1&cid=1.2&en=purchase&gcd={gcd}");
        let r = engine.run(meta_hit(&q));
        assert_eq!(r.tags[1].outcome, "success", "{gcd}: {:?}", r.tags);
        assert_eq!(bodies(&rec, "https://graph.facebook.com").len(), 1, "{gcd}");
    }
}

#[test]
fn send_without_consent_only_covers_missing_signals() {
    const META_NO_CMP: (&str, &[(&str, &str)]) = (
        "meta",
        &[
            ("pixel_id", "123"),
            ("access_token", "tok"),
            ("send_without_consent", "true"),
        ],
    );
    let (engine, rec) = engine_with(providers(&[META_NO_CMP], None), 200);
    let r = engine.run(meta_hit("v=2&tid=G-1&cid=1.2&en=purchase"));
    assert_eq!(r.tags[1].outcome, "success", "{:?}", r.tags);
    assert_eq!(bodies(&rec, "https://graph.facebook.com").len(), 1);

    // A visitor who said no is still never sent.
    let r = engine.run(meta_hit("v=2&tid=G-1&cid=1.2&en=purchase&gcs=G101"));
    assert_eq!(r.tags[1].skip.as_deref(), Some("consent:ad_storage=denied"));
    assert_eq!(bodies(&rec, "https://graph.facebook.com").len(), 1);
}

#[test]
fn a_skipped_tag_reads_as_success_to_event_callbacks() {
    let waits = r#"
        require('addEventCallback')((containerId, eventData) => {
            require('logToConsole')(JSON.stringify(eventData.tags.map(t => t.id + ':' + t.status)));
        });
        data.gtmOnSuccess();
    "#;
    let skips = "data.gtmOnSkipped('because'); data.gtmOnFailure();";
    let (engine, _) = engine_with(
        custom(
            &[("c", RUN_ONE_EVENT), ("a", waits), ("b", skips)],
            &["c"],
            &["a", "b"],
        ),
        204,
    );
    let r = engine.run(ga4_hit("", ""));
    // The first call wins, as with gtmOnSuccess and gtmOnFailure.
    assert_eq!(r.tags[1].outcome, "skipped");
    assert_eq!(r.tags[1].skip.as_deref(), Some("because"));
    assert!(
        r.logs
            .contains(&r#"[a] ["a:success","b:success"]"#.to_string()),
        "{:?}",
        r.logs
    );
}

#[test]
fn a_rejected_send_fails_the_tag() {
    let (engine, _) = engine_answering(
        providers(&[META], None),
        400,
        r#"{"error":{"message":"Invalid OAuth access token"}}"#,
    );
    let r = engine.run(meta_hit(PURCHASE));
    assert_eq!(r.tags[1].outcome, "failure");
    // The debug report says why, without repeating the token.
    let send = r.sends.iter().find(|s| s.from.contains("Meta")).unwrap();
    assert!(send.detail.as_deref().unwrap().contains("Invalid OAuth"));
    assert!(!format!("{send:?}").contains("tok&en"));
}

#[test]
fn enrichment_adds_hashed_fields_from_the_sites_server() {
    // Every send gets the same answer, which the enrichment call and the
    // platform call both receive; only the former is parsed.
    let answer = r#"{"email":" Jane@Example.com ","phone":"+49 170 1234567","first_name":"Jane","country":"de","external_id":"cust-7"}"#;
    let (engine, rec) = engine_answering(
        providers(&[META, TIKTOK], Some("https://shop.example/api/visitor")),
        200,
        answer,
    );
    let r = engine.run(meta_hit(PURCHASE));
    assert!(
        r.tags[1..].iter().all(|t| t.outcome == "success"),
        "{:?} {:?}",
        r.tags,
        r.logs
    );

    let sent = rec.sent.lock().unwrap();
    let lookups: Vec<_> = sent
        .iter()
        .filter(|o| o.url.starts_with("https://shop.example"))
        .collect();
    // Both tags ask at the same time; the identical lookups go out once.
    assert_eq!(lookups.len(), 1);
    assert_eq!(
        lookups[0].url,
        "https://shop.example/api/visitor?user_id=User%40Example.com"
    );
    assert!(
        lookups[0]
            .headers
            .contains(&("x-api-key".into(), "k".into()))
    );
    drop(sent);

    // The next event reuses the answer kept from the first, for both tags.
    engine.run(meta_hit(PURCHASE));
    let again = rec
        .sent
        .lock()
        .unwrap()
        .iter()
        .filter(|o| o.url.starts_with("https://shop.example"))
        .count();
    assert_eq!(again, 1);

    let hex = |s: &str| {
        use sha2::{Digest, Sha256};
        Sha256::digest(s.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    let meta = &bodies(&rec, "https://graph.facebook.com")[0].1["data"][0]["user_data"];
    assert_eq!(meta["em"][0], hex("jane@example.com"));
    assert_eq!(meta["ph"][0], hex("491701234567"));
    assert_eq!(meta["fn"][0], hex("jane"));
    assert_eq!(meta["country"][0], hex("de"));
    assert_eq!(meta["external_id"][0], hex("cust-7"));
    let tt = &bodies(&rec, "https://business-api.tiktok.com")[0].1["data"][0]["user"];
    assert_eq!(tt["email"], hex("jane@example.com"));
    assert_eq!(tt["phone_number"], hex("491701234567"));
}

#[test]
fn enrichment_failure_still_sends_the_conversion() {
    // A 500 from the lookup must not stop the Meta event.
    struct Mixed(Mutex<Vec<Outgoing>>);
    impl Transport for Mixed {
        fn send_all(&self, requests: Vec<Outgoing>) -> Vec<Result<SendResult, String>> {
            requests
                .into_iter()
                .map(|r| {
                    let status = if r.url.starts_with("https://shop.example") {
                        500
                    } else {
                        200
                    };
                    self.0.lock().unwrap().push(r);
                    Ok(SendResult {
                        status,
                        headers: vec![],
                        body: String::new(),
                    })
                })
                .collect()
        }
    }
    let t = Arc::new(Mixed(Mutex::new(vec![])));
    let engine = Engine::new(
        Arc::new(providers(&[META], Some("https://shop.example/api/visitor"))),
        Default::default(),
        t.clone(),
        Duration::from_millis(500),
        32 * 1024 * 1024,
    )
    .unwrap();
    let r = engine.run(meta_hit(PURCHASE));
    assert_eq!(r.tags[1].outcome, "success", "{:?} {:?}", r.tags, r.logs);
    let sent = t.0.lock().unwrap();
    assert!(
        sent.iter()
            .any(|o| o.url.starts_with("https://graph.facebook.com"))
    );
}

#[test]
fn provider_without_its_settings_says_why() {
    let mut c = providers(&[META], None);
    c.tags[1]
        .data
        .as_object_mut()
        .unwrap()
        .remove("access_token");
    let (engine, rec) = engine_with(c, 200);
    let r = engine.run(meta_hit(PURCHASE));
    assert_eq!(r.tags[1].outcome, "failure");
    assert!(
        r.logs.iter().any(|l| l.contains("access_token not set")),
        "{:?}",
        r.logs
    );
    assert!(bodies(&rec, "https://graph.facebook.com").is_empty());
}

#[test]
fn tiktok_refusal_inside_a_200_fails_the_tag() {
    let (engine, _) = engine_answering(
        providers(&[TIKTOK], None),
        200,
        r#"{"code":40001,"message":"Invalid access token"}"#,
    );
    let r = engine.run(ga4_hit(PURCHASE, ""));
    assert_eq!(r.tags[1].outcome, "failure", "{:?}", r.logs);
    assert!(
        r.logs
            .iter()
            .any(|l| l.contains("code 40001 Invalid access token")),
        "{:?}",
        r.logs
    );
}

#[test]
fn a_tag_that_is_off_never_fires() {
    let mut c = Container::builtin();
    c.tags[0].enabled = false;
    let (engine, rec) = engine_with(c.clone(), 204);
    let r = engine.run(ga4_hit("v=2&tid=G-1&cid=1.2&en=page_view", ""));
    assert!(r.tags.is_empty(), "{:?}", r.tags);
    assert!(rec.sent.lock().unwrap().is_empty());
    let t = &c.triggers(&r.events)[0];
    assert!(!t.fired && !t.enabled);
}

/// Meta switched on, with the event's own user data in use or not.
fn meta_with_user_data(on: bool, enrich: Option<&str>) -> Container {
    let mut c = providers(&[META], enrich);
    if on {
        c.tags[1].data["eventUserData"] = json!(true);
    }
    c
}

fn sha_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

const WITH_USER_DATA: &str = "&ep.user_data.email_address=%20Jane%40Example.com%20&ep.user_data.phone_number=%2B49%20170%201234567&ep.user_data.address.first_name=Jane&ep.user_data.address.city=Berlin&ep.user_data.address.postal_code=10115&ep.user_data.address.country=DE";

#[test]
fn event_user_data_is_normalised_and_hashed_before_it_is_sent() {
    let (engine, rec) = engine_with(meta_with_user_data(true, None), 200);
    let r = engine.run(meta_hit(&format!("{PURCHASE}{WITH_USER_DATA}")));
    assert_eq!(r.tags[1].outcome, "success", "{:?} {:?}", r.tags, r.logs);
    let (sent, body) = &bodies(&rec, "https://graph.facebook.com")[0];
    let u = &body["data"][0]["user_data"];
    assert_eq!(u["em"][0], sha_hex("jane@example.com"));
    assert_eq!(u["ph"][0], sha_hex("491701234567"));
    assert_eq!(u["fn"][0], sha_hex("jane"));
    assert_eq!(u["ct"][0], sha_hex("berlin"));
    assert_eq!(u["zp"][0], sha_hex("10115"));
    assert_eq!(u["country"][0], sha_hex("de"));
    // No plain value leaves the server.
    for plain in ["Jane", "jane", "Berlin", "170 1234567", "Example.com"] {
        assert!(!sent.body.contains(plain), "{plain} in {}", sent.body);
    }
}

#[test]
fn hashed_user_data_passes_through_and_tiktok_gets_it_too() {
    let em = sha_hex("a@b.c");
    let ph = sha_hex("4915112345678");
    let mut c = providers(&[META, TIKTOK], None);
    for t in &mut c.tags[1..] {
        t.data["eventUserData"] = json!(true);
    }
    let (engine, rec) = engine_with(c, 200);
    // From the Google tag's envelope (base64) and a tag's sha256_ field (hex).
    let b64 = {
        use base64::Engine as _;
        let bytes: Vec<u8> = (0..32)
            .map(|i| u8::from_str_radix(&em[i * 2..i * 2 + 2], 16).unwrap())
            .collect();
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    };
    let q = format!(
        "{PURCHASE}&em=tv.1~em.{b64}&ep.user_data.sha256_phone_number={}",
        ph.to_uppercase()
    );
    let r = engine.run(meta_hit(&q));
    assert!(
        r.tags[1..].iter().all(|t| t.outcome == "success"),
        "{:?}",
        r.logs
    );
    let meta = &bodies(&rec, "https://graph.facebook.com")[0].1["data"][0]["user_data"];
    assert_eq!(meta["em"][0], em);
    assert_eq!(meta["ph"][0], ph);
    let tt = &bodies(&rec, "https://business-api.tiktok.com")[0].1["data"][0]["user"];
    assert_eq!(tt["email"], em);
    assert_eq!(tt["phone_number"], ph);
}

#[test]
fn event_user_data_is_unused_while_the_switch_is_off() {
    let (engine, rec) = engine_with(meta_with_user_data(false, None), 200);
    engine.run(meta_hit(&format!("{PURCHASE}{WITH_USER_DATA}")));
    let u = &bodies(&rec, "https://graph.facebook.com")[0].1["data"][0]["user_data"];
    for k in ["em", "ph", "fn", "ct", "zp", "country"] {
        assert!(u.get(k).is_none(), "{k}: {u}");
    }
}

#[test]
fn the_enrichment_answer_wins_field_by_field() {
    let answer = r#"{"email":"other@example.com"}"#;
    let (engine, rec) = engine_answering(
        meta_with_user_data(true, Some("https://shop.example/api/visitor")),
        200,
        answer,
    );
    engine.run(meta_hit(&format!("{PURCHASE}{WITH_USER_DATA}")));
    let u = &bodies(&rec, "https://graph.facebook.com")[0].1["data"][0]["user_data"];
    assert_eq!(u["em"][0], sha_hex("other@example.com"));
    assert_eq!(u["ph"][0], sha_hex("491701234567"));
}

#[test]
fn ga4_tag_strips_plain_user_data_only_when_asked() {
    let query = "v=2&tid=G-TEST000001&cid=111.222&en=sign_up\
        &ep.user_data.email_address=a%40b.c&epn.user_data.address.postal_code=10115\
        &ep.user_data.sha256_phone_number=abc123&ep.user_data._tag_mode=MANUAL\
        &em=tv.1~em.digest&ep.plan=pro";
    let sent_url = |strip: bool| {
        let mut c = Container::builtin_with(&Settings {
            measurement_ids: vec!["G-TEST000001".into()],
            strip_user_data: strip,
            ..Default::default()
        });
        c.compile().unwrap();
        let (engine, rec) = engine_with(c, 204);
        let r = engine.run(ga4_hit(query, ""));
        assert_eq!(r.tags[0].outcome, "success", "{:?}", r.logs);
        rec.sent.lock().unwrap()[0].url.clone()
    };

    let kept = sent_url(false);
    assert!(
        kept.contains("ep.user_data.email_address=a%40b.c"),
        "{kept}"
    );

    let url = sent_url(true);
    for gone in [
        "ep.user_data.email_address",
        "epn.user_data.address.postal_code",
    ] {
        assert!(!url.contains(gone), "{gone} in {url}");
    }
    for kept in [
        "ep.user_data.sha256_phone_number=abc123",
        "ep.user_data._tag_mode=MANUAL",
        "em=tv.1%7Eem.digest",
        "ep.plan=pro",
        "en=sign_up",
    ] {
        assert!(url.contains(kept), "{kept} missing from {url}");
    }
}

const GOOGLE_ADS: (&str, &[(&str, &str)]) = (
    "google_ads",
    &[
        ("customer_id", "123-456-7890"),
        ("conversion_action_id", "555"),
        ("client_id", "cid.apps.googleusercontent.com"),
        ("client_secret", "sec&ret"),
        ("refresh_token", "1//rt"),
        ("test_mode", "true"),
    ],
);
#[test]
fn kept_answers_hide_secret_values() {
    assert_eq!(
        super::without_secrets(
            r#"{"access_token": "ya29.abc\"def", "expires_in": 3599, "Token":"t", "token_type":"Bearer"}"#
        ),
        r#"{"access_token": "[redacted]", "expires_in": 3599, "Token":"[redacted]", "token_type":"Bearer"}"#
    );
    assert_eq!(
        super::without_secrets("access_token=abc&scope=x&refresh_token=r"),
        "access_token=[redacted]&scope=x&refresh_token=[redacted]"
    );
    assert_eq!(
        super::without_secrets(r#"{"error":"invalid_grant"}"#),
        r#"{"error":"invalid_grant"}"#
    );
}

const GOOGLE_ANSWER: &str = r#"{"access_token":"at-1","expires_in":3600,"requestId":"r1"}"#;

fn with_cookie(mut req: IncomingRequest, cookie: &str) -> IncomingRequest {
    req.headers.retain(|(k, _)| k != "cookie");
    req.headers.push(("cookie".into(), cookie.into()));
    req
}

#[test]
fn google_ads_keeps_the_ad_click_in_a_first_party_cookie() {
    let (engine, rec) = engine_answering(providers(&[GOOGLE_ADS], None), 200, GOOGLE_ANSWER);
    engine.set_clock(Some(1_792_915_205_000));
    let r = engine.run(ga4_hit(
        "v=2&tid=G-1&cid=1.2&en=page_view&dl=https%3A%2F%2Fexample.com%2F%3Fgclid%3DCj0abc.d&gcd=13r3r3r2r5l1",
        "",
    ));
    let cookie = set_cookies(&r)
        .into_iter()
        .find(|c| c.starts_with("_sst_ads="))
        .expect("cookie");
    assert!(
        cookie.starts_with("_sst_ads=v1.gclid.1792915205.Cj0abc.d;"),
        "{cookie}"
    );
    assert!(
        cookie.contains("Domain=example.com")
            && cookie.contains("Max-Age=7776000")
            && cookie.contains("Secure"),
        "{cookie}"
    );
    assert!(
        bodies(&rec, "https://datamanager").is_empty(),
        "a page view is no conversion"
    );

    // Denied ad storage: no cookie.
    let r = engine.run(ga4_hit(
        "v=2&tid=G-1&cid=1.2&en=page_view&gcs=G100&dl=https%3A%2F%2Fexample.com%2F%3Fgclid%3DX",
        "",
    ));
    assert!(!set_cookies(&r).iter().any(|c| c.starts_with("_sst_ads=")));

    // A "click id" carrying cookie attributes isn't one.
    let r = engine.run(ga4_hit(
        "v=2&tid=G-1&cid=1.2&en=page_view&dl=https%3A%2F%2Fexample.com%2F%3Fgclid%3DX%253B%2520Domain%253Devil.example&gcd=13r3r3r2r5l1",
        "",
    ));
    assert!(
        !set_cookies(&r).iter().any(|c| c.starts_with("_sst_ads=")),
        "{:?}",
        set_cookies(&r)
    );
}

#[test]
fn google_ads_sends_a_conversion_with_the_click_and_a_cached_token() {
    let (engine, rec) = engine_answering(providers(&[GOOGLE_ADS], None), 200, GOOGLE_ANSWER);
    engine.set_clock(Some(1_792_915_205_000));
    let req = with_cookie(
        ga4_hit(PURCHASE, ""),
        "_sst_ads=v1.gclid.1792900000.Cj0abc.d; _gcl_aw=GCL.1.other",
    );
    let r = engine.run(req.clone());
    let tag = r
        .tags
        .iter()
        .find(|t| t.tag == "Google Ads conversions")
        .unwrap();
    assert_eq!(tag.outcome, "success", "{:?}", r.logs);
    // Google's answer is kept with the hit, without its access token.
    let kept = serde_json::to_string(&r.sends).unwrap();
    assert!(!kept.contains("at-1"), "{kept}");
    assert!(kept.contains("[redacted]"), "{kept}");
    let sent = rec.sent.lock().unwrap().clone();
    let token = sent
        .iter()
        .find(|o| o.url == "https://oauth2.googleapis.com/token")
        .expect("token request");
    assert!(
        token.body.contains("grant_type=refresh_token")
            && token.body.contains("client_secret=sec%26ret"),
        "{}",
        token.body
    );
    let ingest = bodies(&rec, "https://datamanager.googleapis.com/v1/events:ingest");
    assert_eq!(ingest.len(), 1);
    let (out, body) = &ingest[0];
    assert!(
        out.headers
            .contains(&("authorization".into(), "Bearer at-1".into())),
        "{:?}",
        out.headers
    );
    let d = &body["destinations"][0];
    assert_eq!(
        d["operatingAccount"],
        json!({ "accountType": "GOOGLE_ADS", "accountId": "1234567890" })
    );
    assert_eq!(d["productDestinationId"], "555");
    assert_eq!(body["validateOnly"], true);
    let ev = &body["events"][0];
    assert_eq!(
        ev["adIdentifiers"]["gclid"], "Cj0abc.d",
        "our cookie wins over Google's"
    );
    assert_eq!(ev["conversionValue"], 19.9);
    assert_eq!(ev["currency"], "EUR");
    assert_eq!(ev["transactionId"], "T1");
    assert_eq!(ev["eventTimestamp"], "2026-10-25T08:00:05+00:00");
    assert_eq!(ev["eventSource"], "WEB");

    // The token is reused by the next conversion.
    engine.run(req);
    let tokens = rec
        .sent
        .lock()
        .unwrap()
        .iter()
        .filter(|o| o.url.contains("oauth2"))
        .count();
    assert_eq!(tokens, 1);
}

#[test]
fn google_ads_matches_on_user_data_without_a_click() {
    let mut c = Container::builtin();
    c.tags.push(serde_json::from_value(json!({
        "name": "Ads lead", "template": "google_ads",
        "data": { "customer_id": "1", "conversion_action_id": "2", "client_id": "c", "client_secret": "s", "refresh_token": "r",
                  "conversion_event": "generate_lead", "eventUserData": true }
    })).unwrap());
    c.compile().unwrap();
    let (engine, rec) = engine_answering(c, 200, GOOGLE_ANSWER);
    let q = "v=2&tid=G-1&cid=1.2&en=generate_lead&ep.user_data.email_address=Jane.Doe%40GMail.com&ep.user_data.phone_number=%2B49%20170%201234567\
             &ep.user_data.address.first_name=Jane&ep.user_data.address.last_name=Doe&ep.user_data.address.country=de&ep.user_data.address.postal_code=10115\
             &dl=https%3A%2F%2Fexample.com%2Fcontact&gcd=13r3r3r2r5l1";
    let r = engine.run(ga4_hit(q, ""));
    assert!(
        r.tags.iter().all(|t| t.outcome == "success"),
        "{:?} {:?}",
        r.tags,
        r.logs
    );
    let ingest = bodies(&rec, "https://datamanager");
    let ev = &ingest[0].1["events"][0];
    assert!(ev.get("adIdentifiers").is_none());
    assert!(
        ev["transactionId"].as_str().unwrap().starts_with("1.2."),
        "falls back to client id and time"
    );
    let ids = &ev["userData"]["userIdentifiers"];
    assert_eq!(
        ids[0]["emailAddress"], "d6117306485ed0e50afab3ac871e98f81699151f30281527d63ff5f233656c69",
        "gmail dots removed"
    );
    assert_eq!(
        ids[1]["phoneNumber"], "b4267a2234e2072fa4d2e8d89fbe727c50fc414731d6ecacc2ed3d00f05038d1",
        "E.164 with +"
    );
    assert_eq!(
        ids[2]["address"],
        json!({
            "givenName": "81f8f6dde88365f3928796ec7aa53f72820b06db8664f5fe76a7eb13e24546a2",
            "familyName": "799ef92a11af918e3fb741df42934f3b568ed2d93ac1df74f1b8d41a27932a6f",
            "regionCode": "DE", "postalCode": "10115",
        })
    );
    assert!(ingest[0].1.get("validateOnly").is_none());

    // Nothing to match with: nothing sent, and that isn't a failure.
    let r = engine.run(ga4_hit(
        "v=2&tid=G-1&cid=1.2&en=generate_lead&dl=https%3A%2F%2Fexample.com%2F&gcd=13r3r3r2r5l1",
        "",
    ));
    assert!(r.tags.iter().all(|t| t.outcome == "success"));
    assert!(
        r.logs
            .iter()
            .any(|l| l.contains("no ad click and no user data")),
        "{:?}",
        r.logs
    );
    assert_eq!(bodies(&rec, "https://datamanager").len(), 1);
}

#[test]
fn google_ads_reports_google_refusals() {
    let (engine, rec) = engine_answering(
        providers(&[GOOGLE_ADS], None),
        400,
        r#"{"error":{"message":"invalid_grant"}}"#,
    );
    let r = engine.run(with_cookie(
        ga4_hit(PURCHASE, ""),
        "_sst_ads=v1.gbraid.1.GB1",
    ));
    let tag = r
        .tags
        .iter()
        .find(|t| t.tag == "Google Ads conversions")
        .unwrap();
    assert_eq!(tag.outcome, "failure");
    assert!(
        r.logs
            .iter()
            .any(|l| l.contains("refused the refresh token") && l.contains("invalid_grant")),
        "{:?}",
        r.logs
    );
    assert!(bodies(&rec, "https://datamanager").is_empty());
}

#[test]
fn utc_timestamps_cover_leap_days() {
    let (engine, rec) = engine_answering(providers(&[GOOGLE_ADS], None), 200, GOOGLE_ANSWER);
    engine.set_clock(Some(1_709_251_199_000));
    engine.run(with_cookie(
        ga4_hit(PURCHASE, ""),
        "_sst_ads=v1.wbraid.1.WB1",
    ));
    let ingest = bodies(&rec, "https://datamanager");
    assert_eq!(
        ingest[0].1["events"][0]["eventTimestamp"],
        "2024-02-29T23:59:59+00:00"
    );
    assert_eq!(ingest[0].1["events"][0]["adIdentifiers"]["wbraid"], "WB1");
}

const BIGQUERY: (&str, &[(&str, &str)]) = (
    "bigquery",
    &[
        ("project_id", "my-project"),
        ("dataset_id", "analytics"),
        ("table_id", "events"),
    ],
);
const BIGQUERY_URL: &str = "https://bigquery.googleapis.com/bigquery/v2/projects/my-project/datasets/analytics/tables/events/insertAll";

#[test]
fn bigquery_writes_a_row_with_the_servers_token_and_no_ip() {
    let (engine, rec) = engine_with(
        providers(&[BIGQUERY], Some("https://shop.example/lookup")),
        200,
    );
    let r = engine.run(meta_hit(&format!(
        "{PURCHASE}&dr=https%3A%2F%2Fsearch.example%2F"
    )));
    assert_eq!(r.tags[1].outcome, "success", "{:?} {:?}", r.tags, r.logs);
    let sent = bodies(&rec, "https://bigquery.googleapis.com");
    assert_eq!(sent.len(), 1);
    let (out, body) = &sent[0];
    assert_eq!(out.url, BIGQUERY_URL);
    assert_eq!(out.method, "POST");
    // The transport adds the token; the template never had one.
    assert!(out.google_auth);
    assert!(!out.headers.iter().any(|(k, _)| k == "authorization"));
    assert!(!out.body.contains("203.0.113.9"), "{}", out.body);
    assert_eq!(body["ignoreUnknownValues"], true);
    let row = &body["rows"][0];
    assert_eq!(row["insertId"].as_str().unwrap().len(), 32);
    let json = &row["json"];
    assert_eq!(json["event_name"], "purchase");
    assert_eq!(json["event_id"], "ev-1");
    assert_eq!(json["client_id"], "1.2");
    assert_eq!(json["user_id"], "User@Example.com");
    assert_eq!(json["measurement_id"], "G-1");
    assert_eq!(json["value"], 19.9);
    assert_eq!(json["currency"], "EUR");
    assert_eq!(json["transaction_id"], "T1");
    assert_eq!(json["page_location"], "https://example.com/thanks");
    assert_eq!(json["page_referrer"], "https://search.example/");
    assert_eq!(json["user_agent"], "Mozilla/5.0 Test");
    assert!(json["event_timestamp"].as_f64().unwrap() > 1.7e9);
    let consent: serde_json::Value =
        serde_json::from_str(json["consent"].as_str().unwrap()).unwrap();
    assert_eq!(consent["analytics_storage"], "granted");
    let all: serde_json::Value =
        serde_json::from_str(json["event_data"].as_str().unwrap()).unwrap();
    assert_eq!(all["transaction_id"], "T1");
    assert!(all.get("ip_override").is_none() && all.get("x-sst-raw-hit").is_none());
    // The export doesn't look the visitor up.
    assert!(bodies(&rec, "https://shop.example").is_empty());
}

#[test]
fn bigquery_writes_items_as_json() {
    let (engine, rec) = engine_with(providers(&[BIGQUERY], None), 200);
    let r = engine.run(meta_hit(&format!("{PURCHASE}&pr1=idSKU-1~nmShoe~pr10~qt2")));
    assert_eq!(r.tags[1].outcome, "success", "{:?}", r.logs);
    let sent = bodies(&rec, "https://bigquery.googleapis.com");
    let items: serde_json::Value =
        serde_json::from_str(sent[0].1["rows"][0]["json"]["items"].as_str().unwrap()).unwrap();
    assert_eq!(items[0]["item_id"], "SKU-1");
    assert_eq!(items[0]["quantity"], 2);
}

#[test]
fn bigquery_skips_unselected_events_and_denied_analytics() {
    const BASE: &str = "v=2&tid=G-1&cid=1.2&en=purchase";
    let only_purchases: (&str, &[(&str, &str)]) = (
        "bigquery",
        &[
            ("project_id", "my-project"),
            ("dataset_id", "analytics"),
            ("table_id", "events"),
            ("events", "generate_lead, purchase"),
        ],
    );
    for (q, setup, skip) in [
        (
            "v=2&tid=G-1&cid=1.2&en=page_view&gcs=G111".to_string(),
            only_purchases,
            Some("event_not_selected"),
        ),
        (format!("{BASE}&gcs=G111"), only_purchases, None),
        // Ad storage denied doesn't matter here; analytics storage does.
        (format!("{BASE}&gcs=G101"), BIGQUERY, None),
        (
            format!("{BASE}&gcs=G110"),
            BIGQUERY,
            Some("consent:analytics_storage=denied"),
        ),
        (
            format!("{BASE}&gcd=13r3p3r2r5l1"),
            BIGQUERY,
            Some("consent:analytics_storage=denied"),
        ),
        (
            BASE.to_string(),
            BIGQUERY,
            Some("consent:analytics_storage=missing"),
        ),
    ] {
        let (engine, rec) = engine_with(providers(&[setup], None), 200);
        let r = engine.run(meta_hit(&q));
        let sent = bodies(&rec, "https://bigquery.googleapis.com").len();
        match skip {
            Some(reason) => {
                assert_eq!(r.tags[1].outcome, "skipped", "{q}: {:?}", r.tags);
                assert_eq!(r.tags[1].skip.as_deref(), Some(reason), "{q}");
                assert_eq!(sent, 0, "{q}");
            }
            None => {
                assert_eq!(r.tags[1].outcome, "success", "{q}: {:?}", r.logs);
                assert_eq!(sent, 1, "{q}");
            }
        }
    }

    let anyway: (&str, &[(&str, &str)]) = (
        "bigquery",
        &[
            ("project_id", "my-project"),
            ("dataset_id", "analytics"),
            ("table_id", "events"),
            ("send_without_consent", "true"),
        ],
    );
    let (engine, rec) = engine_with(providers(&[anyway], None), 200);
    let r = engine.run(meta_hit(BASE));
    assert_eq!(r.tags[1].outcome, "success", "{:?}", r.logs);
    assert_eq!(bodies(&rec, "https://bigquery.googleapis.com").len(), 1);
    let (engine, rec) = engine_with(providers(&[anyway], None), 200);
    let r = engine.run(meta_hit(&format!("{BASE}&gcs=G110")));
    assert_eq!(r.tags[1].outcome, "skipped");
    assert!(bodies(&rec, "https://bigquery.googleapis.com").is_empty());
}

#[test]
fn bigquery_row_errors_and_refusals_fail_the_tag() {
    for (status, answer, logged) in [
        (
            200,
            r#"{"insertErrors":[{"index":0,"errors":[{"reason":"invalid","message":"no such field: plan"}]}]}"#,
            "BigQuery insert failed: invalid no such field: plan",
        ),
        (
            404,
            r#"{"error":{"code":404,"message":"Not found: Table my-project:analytics.events","errors":[{"reason":"notFound"}]}}"#,
            "BigQuery insert failed: notFound Not found: Table",
        ),
        (403, "", "BigQuery insert failed: http_403"),
    ] {
        let (engine, _) = engine_answering(providers(&[BIGQUERY], None), status, answer);
        let r = engine.run(meta_hit(PURCHASE));
        assert_eq!(r.tags[1].outcome, "failure", "{status}: {:?}", r.tags);
        assert!(
            r.logs.iter().any(|l| l.contains(logged)),
            "{status}: {:?}",
            r.logs
        );
    }
}

#[test]
fn bigquery_insert_checks_ids_rows_and_the_permitted_tables() {
    let src = r#"
        const BigQuery = require('BigQuery');
        BigQuery.insert(
          { projectId: data.p, datasetId: 'analytics', tableId: data.t },
          data.rows || [{ a: 1 }]
        ).then(data.gtmOnSuccess, (e) => data.gtmOnSkipped(e[0].reason + ':' + (e[0].message || '')));
    "#;
    let run = |tag_data: serde_json::Value| {
        let mut c = permitted_tag(
            src,
            json!({"access_bigquery": {"tables": ["my-project.analytics.*"]}}),
        );
        c.tags[0].data = tag_data;
        let (engine, rec) = engine_with(c, 200);
        let r = engine.run(ga4_hit(HIT, ""));
        let sent: Vec<Outgoing> = rec.sent.lock().unwrap().clone();
        (r, sent)
    };

    let (r, sent) = run(json!({"p": "my-project", "t": "events$20261015"}));
    assert_eq!(r.tags[0].outcome, "success", "{:?}", r.logs);
    assert_eq!(
        sent[0].url,
        "https://bigquery.googleapis.com/bigquery/v2/projects/my-project/datasets/analytics/tables/events%2420261015/insertAll"
    );
    assert!(sent[0].google_auth);

    // A table the permission doesn't list throws.
    let (r, sent) = run(json!({"p": "other-project", "t": "events"}));
    assert_eq!(r.tags[0].outcome, "failure");
    assert!(
        r.tags[0]
            .error
            .as_deref()
            .unwrap_or("")
            .contains("not allowed")
    );
    assert!(sent.is_empty());

    // Ids that could change the URL, and rows that aren't objects, are refused
    // before anything is sent.
    for (d, reason) in [
        (
            json!({"p": "my-project", "t": "events/../x"}),
            "invalid:connectionInfo",
        ),
        (
            json!({"p": "my-project", "t": "events?x=1"}),
            "invalid:connectionInfo",
        ),
        (
            json!({"p": "my-project", "t": "e", "rows": []}),
            "invalid:rows",
        ),
        (
            json!({"p": "my-project", "t": "e", "rows": [1]}),
            "invalid:rows",
        ),
    ] {
        let mut c = permitted_tag(src, json!({"access_bigquery": {"tables": ["*"]}}));
        c.tags[0].data = d.clone();
        let (engine, rec) = engine_with(c, 200);
        let r = engine.run(ga4_hit(HIT, ""));
        assert!(
            r.tags[0].skip.as_deref().unwrap_or("").starts_with(reason),
            "{d}: {:?} {:?}",
            r.tags,
            r.logs
        );
        assert!(rec.sent.lock().unwrap().is_empty(), "{d}");
    }
}
