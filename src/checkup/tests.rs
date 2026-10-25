use std::collections::{BTreeMap, BTreeSet};

use axum::Router;
use axum::routing::get;
use serde_json::{Value, json};

use super::*;
use crate::counts::{Counts, Runs};

fn kinds(items: &[Value]) -> Vec<(String, String)> {
    items
        .iter()
        .map(|i| {
            (
                i["kind"].as_str().unwrap().to_string(),
                i["status"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn has(items: &[Value], kind: &str, status: &str) -> bool {
    kinds(items).contains(&(kind.to_string(), status.to_string()))
}

fn find<'a>(items: &'a [Value], kind: &str) -> &'a Value {
    items
        .iter()
        .find(|i| i["kind"] == kind)
        .unwrap_or_else(|| panic!("no {kind} in {items:?}"))
}

fn container(tags: Value) -> Value {
    json!({ "tags": tags })
}

#[test]
fn scans_pages_and_containers() {
    let gtm = r#"var data = {"resource":{"tags":[{"function":"__googtag","vtp_tagId":"G-ABC12345","vtp_configSettingsTable":["list",["map","parameter","server_container_url","parameterValue","https:\/\/shop.example.com\/metrics"]]},{"function":"__html","vtp_html":"<script>!function(f,b,e,v){...}(window,document,'script','https://connect.facebook.net/en_US/fbevents.js');fbq('init','1');</script>"},{"function":"__awct","vtp_conversionId":"AW-1234567890"}]}}"#;
    let s = scan(gtm);
    assert_eq!(s["ga4_ids"], json!(["G-ABC12345"]));
    assert_eq!(
        s["server_urls"],
        json!(["https://shop.example.com/metrics"])
    );
    assert_eq!(s["pixels"], json!(["meta", "google_ads"]));

    let page = r#"<script src="https://shop.example.com/metrics/gtm.js?id=GTM-WXYZ12"></script>
      <script>gtag('config', 'G-ZZZ99999', { server_container_url: 'https://shop.example.com/metrics' }); ttq.load('C1');</script>"#;
    let s = scan(page);
    assert_eq!(s["gtm_ids"], json!(["GTM-WXYZ12"]));
    assert_eq!(
        s["server_urls"],
        json!(["https://shop.example.com/metrics"])
    );
    assert_eq!(s["pixels"], json!(["tiktok"]));
}

#[test]
fn nothing_arriving_is_the_first_thing_said() {
    let items = check(
        &container(json!([{ "name": "GA4", "template": "ga4_tag", "data": {} }])),
        &Counts::default(),
        &[],
        None,
    );
    assert!(has(&items, "no_hits", "bad"));
    assert!(has(&items, "no_ad_platforms", "info"));
    assert!(
        !kinds(&items).iter().any(|(k, _)| k == "tag_idle"),
        "no hits, so idle says nothing"
    );
}

fn purchase_counts(n: u64) -> Counts {
    let mut c = Counts {
        hits: 100,
        ..Default::default()
    };
    c.events.insert("purchase".into(), n);
    c.events.insert("page_view".into(), 90);
    let mut m = BTreeMap::new();
    m.insert("currency".to_string(), n);
    m.insert("event_id".to_string(), n);
    c.missing.insert("purchase".into(), m);
    c
}

#[test]
fn tags_are_checked_for_settings_failures_and_idleness() {
    let tags = json!([
        { "name": "GA4", "template": "ga4_tag", "data": {} },
        { "name": "Meta", "template": "meta_capi", "data": { "pixel_id": "1", "access_token": "[redacted]", "test_event_code": "TEST1" } },
        { "name": "TikTok", "template": "tiktok_events", "data": { "pixel_code": "C1", "access_token": "" } },
        { "name": "Ads", "template": "google_ads", "data": { "customer_id": "1", "conversion_action_id": "2", "client_id": "c", "client_secret": "[redacted]", "refresh_token": "[redacted]", "eventUserData": true } },
        { "name": "Off", "template": "ga4_tag", "data": {}, "enabled": false },
    ]);
    let mut c = purchase_counts(10);
    c.tags.insert(
        "GA4".into(),
        Runs {
            runs: 100,
            failed: 0,
        },
    );
    c.tags.insert(
        "Meta".into(),
        Runs {
            runs: 10,
            failed: 5,
        },
    );
    let failing_hit = json!({ "result": {
        "events": [{ "event_name": "purchase", "page_location": "https://shop/?gclid=abc", "x-ga-gcs": "G111" }],
        "tags": [{ "tag": "Meta", "outcome": "failure" }],
        "logs": ["[GA4] ok", "[Meta] not sent: Invalid OAuth access token"],
    }});
    let items = check(
        &container(tags),
        &c,
        &[failing_hit],
        Some(&json!({ "pixels": ["meta"] })),
    );
    assert!(has(&items, "hits", "ok"));
    assert_eq!(
        find(&items, "tag_settings")["params"],
        json!({ "tag": "TikTok", "settings": ["Access token"] })
    );
    let f = find(&items, "tag_failing");
    assert_eq!(f["params"]["tag"], "Meta");
    assert_eq!(
        f["params"]["why"],
        "[Meta] not sent: Invalid OAuth access token"
    );
    assert_eq!(find(&items, "test_mode")["params"]["tag"], "Meta");
    assert_eq!(find(&items, "tag_idle")["params"]["tag"], "Ads");
    assert!(has(&items, "tag_ok", "ok"));
    assert!(
        !items.iter().any(|i| i["params"]["tag"] == "Off"),
        "a tag that is off isn't checked"
    );
    // Data: currency missing on every purchase, said once for three tags.
    assert_eq!(
        items
            .iter()
            .filter(|i| i["kind"] == "value_missing" && i["params"]["key"] == "currency")
            .count(),
        1
    );
    // Meta also runs in the browser, so event_id matters.
    let e = find(&items, "event_id_missing");
    assert_eq!(
        (e["status"].as_str(), e["params"]["pixel"].as_bool()),
        (Some("warn"), Some(true))
    );
    assert!(has(&items, "user_data_missing", "warn"));
    assert!(has(&items, "ad_clicks", "ok"));
}

#[test]
fn conversions_the_site_never_sends_are_named() {
    let tags = json!([{ "name": "Ads lead", "template": "google_ads", "data": { "conversion_event": "generate_lead" } }]);
    let items = check(&container(tags), &purchase_counts(5), &[], None);
    assert_eq!(
        find(&items, "conversion_missing")["params"],
        json!({ "tag": "Ads lead", "event": "generate_lead" })
    );
}

#[test]
fn warns_about_server_addresses_blockers_match() {
    let items_for = |site: &str, url: &str| {
        let web = json!({ "site": site, "gtm_id": "GTM-1", "public_url": url,
            "server_urls": [url], "ga4_ids": [], "pixels": [], "first_party_gtm": false });
        check(&container(json!([])), &Counts::default(), &[], Some(&web))
    };
    let words = |site: &str, url: &str| -> Vec<Value> {
        items_for(site, url)
            .into_iter()
            .filter(|i| i["kind"] == "server_url_word")
            .map(|i| i["params"]["word"].clone())
            .collect()
    };
    assert_eq!(
        words(
            "https://shop.example.com",
            "https://shop.example.com/metrics"
        ),
        [json!("metrics")]
    );
    assert_eq!(
        words("https://shop.example.com", "https://gtm.example.com/"),
        [json!("gtm")]
    );
    assert!(words("https://shop.example.com", "https://shop.example.com/k7q2").is_empty());
    // The site's own name doesn't count.
    assert!(words("https://ad-house.example", "https://ad-house.example/k7q2").is_empty());
}

#[test]
fn the_web_side_must_send_here() {
    let web = json!({ "site": "https://shop.example.com", "gtm_id": "GTM-1", "public_url": "https://shop.example.com/metrics",
        "server_urls": ["https://sgtm.other.com"], "ga4_ids": ["G-WEB0001"], "pixels": [], "first_party_gtm": false });
    let hit = json!({ "result": { "events": [{ "event_name": "page_view", "x-ga-measurement_id": "G-HIT0001" }] } });
    let items = check(
        &container(json!([])),
        &Counts {
            hits: 1,
            ..Default::default()
        },
        &[hit],
        Some(&web),
    );
    assert!(has(&items, "server_url_elsewhere", "bad"));
    assert!(has(&items, "ga4_ids_differ", "warn"));
    assert_eq!(find(&items, "server_url_word")["params"]["word"], "sgtm");

    let mut web2 = web.clone();
    web2["server_urls"] = json!(["https://shop.example.com/metrics/"]);
    let items = check(
        &container(json!([])),
        &Counts {
            hits: 1,
            ..Default::default()
        },
        &[],
        Some(&web2),
    );
    assert!(has(&items, "server_url", "ok"));
    web2["server_urls"] = json!([]);
    assert!(has(
        &check(&container(json!([])), &Counts::default(), &[], Some(&web2)),
        "server_url_missing",
        "bad"
    ));
    let down = json!({ "site": "https://x", "site_error": "HTTP 503" });
    assert!(has(
        &check(&container(json!([])), &Counts::default(), &[], Some(&down)),
        "site_unreachable",
        "bad"
    ));
}

#[tokio::test]
async fn reads_the_site_and_its_web_container() {
    let app = Router::new()
        .route("/", get(|| async { r#"<html><script async src="https://www.googletagmanager.com/gtm.js?id=GTM-ABCD12"></script></html>"# }))
        .route("/gtm.js", get(|| async { r#"{"vtp_tagId":"G-ABC12345","parameter","server_container_url","parameterValue","https:\/\/shop.example.com\/metrics"} fbq('init')"# }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    let w = analyse_from(
        &client(true),
        &format!("{base}/"),
        None,
        Some("https://shop.example.com/metrics"),
        &base,
    )
    .await;
    assert_eq!(w["gtm_id"], "GTM-ABCD12");
    assert_eq!(w["ga4_ids"], json!(["G-ABC12345"]));
    assert_eq!(
        w["server_urls"],
        json!(["https://shop.example.com/metrics"])
    );
    assert_eq!(w["pixels"], json!(["meta"]));
    assert_eq!(w["first_party_gtm"], false);
    let w = analyse_from(&client(true), &format!("{base}/nope"), None, None, &base).await;
    assert_eq!(w["site_error"], "HTTP 404");
    // Private addresses are refused without OUTGOING_ALLOW_PRIVATE.
    let w = analyse_from(&client(false), &format!("{base}/"), None, None, &base).await;
    assert!(w["site_error"].is_string());
}

#[test]
fn a_live_check_follows_one_browser_by_its_client_id() {
    let hit = |at: u64, cid: &str, name: &str, url: &str, extra: Value| {
        let mut e = json!({ "event_name": name, "client_id": cid, "page_location": url, "x-ga-gcs": "G111" });
        if let (Some(m), Value::Object(x)) = (e.as_object_mut(), extra) {
            m.extend(x);
        }
        json!({ "at": at, "result": { "events": [e], "tags": [{ "tag": "Meta", "event": 0, "outcome": "success" }], "sends": [{ "result": { "Ok": 200 } }, { "result": { "Err": "timed_out" } }] } })
    };
    // Newest first, as the trace buffer lists them.
    let hits = vec![
        (
            5,
            hit(
                500,
                "1.2",
                "purchase",
                "https://shop/thanks",
                json!({ "value": 9.5, "currency": "TRY", "transaction_id": "O1" }),
            ),
        ),
        (4, hit(400, "9.9", "page_view", "https://shop/", json!({}))),
        (
            3,
            hit(
                300,
                "1.2",
                "page_view",
                "https://shop/?sst_check=ab12cd34&gclid=x",
                json!({}),
            ),
        ),
        (
            2,
            hit(100, "1.2", "page_view", "https://shop/old", json!({})),
        ),
    ];
    let v = live("ab12cd34", 200, &hits, &json!({}), &BTreeSet::new());
    assert_eq!(v["found"], true);
    let traces: Vec<u64> = v["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["trace"].as_u64().unwrap())
        .collect();
    assert_eq!(
        traces,
        vec![3, 5],
        "oldest first, other browsers and earlier hits left out"
    );
    let first = &v["hits"][0]["events"][0];
    assert_eq!(first["has"]["ad_click"], true);
    let buy = &v["hits"][1];
    assert_eq!(buy["events"][0]["has"]["value"], true);
    assert_eq!(buy["events"][0]["has"]["event_id"], false);
    assert_eq!(buy["events"][0]["tags"][0]["tag"], "Meta");
    assert_eq!(
        (buy["sends"].as_u64(), buy["sends_failed"].as_u64()),
        (Some(2), Some(1))
    );
    assert_eq!(
        live("00000000", 0, &hits, &json!({}), &BTreeSet::new())["found"],
        false
    );
}

#[test]
fn a_test_conversion_gets_a_verdict_per_tag() {
    let container = json!({ "tags": [
        { "name": "GA4", "template": "ga4_tag", "data": {} },
        { "name": "Meta", "template": "meta_capi", "data": { "pixel_id": "1", "access_token": "[redacted]", "eventUserData": true } },
        { "name": "Ads", "template": "google_ads", "data": {} },
        { "name": "TikTok", "template": "tiktok_events", "data": {} },
        { "name": "Lead ads", "template": "google_ads", "data": { "conversion_event": "generate_lead" } },
    ]});
    let landing = json!({ "at": 300, "request": { "headers": [["cookie", "_ga"]] }, "result": {
        "events": [{ "event_name": "page_view", "client_id": "1.2", "page_location": "https://shop/?sst_check=ab12cd34" }],
        "tags": [], "sends": [], "logs": [] } });
    let waiting = live(
        "ab12cd34",
        0,
        &[(1, landing.clone())],
        &container,
        &BTreeSet::new(),
    );
    assert_eq!(
        waiting["verdict"]["waiting"],
        json!(["generate_lead", "purchase"])
    );

    let buy = json!({ "at": 400, "request": { "headers": [["cookie", "_ga, _sst_ads"]] }, "result": {
        "events": [{ "event_name": "purchase", "client_id": "1.2", "page_location": "https://shop/thanks", "value": 5, "currency": "TRY" }],
        "tags": [
            { "tag": "GA4", "event": 0, "outcome": "success" },
            { "tag": "Meta", "event": 0, "outcome": "success" },
            { "tag": "Ads", "event": 0, "outcome": "failure" },
            { "tag": "TikTok", "event": 0, "outcome": "success" },
        ],
        "sends": [
            { "from": "GA4", "result": { "Ok": 204 } },
            { "from": "Meta", "result": { "Ok": 200 } },
            { "from": "TikTok", "result": { "Ok": 400 } },
        ],
        "logs": ["[Ads] not sent: client_id and client_secret and refresh_token not set"],
    }});
    let mut pixels = BTreeSet::new();
    pixels.insert("meta".to_string());
    let v = live(
        "ab12cd34",
        0,
        &[(2, buy), (1, landing)],
        &container,
        &pixels,
    );
    let vd = &v["verdict"];
    assert_eq!(vd["event"], "purchase");
    assert_eq!(vd["trace"], 2);
    let tag = |n: &str| {
        vd["tags"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["tag"] == n)
            .cloned()
    };
    assert_eq!(tag("GA4").unwrap()["status"], "pass");
    let meta = tag("Meta").unwrap();
    assert_eq!(meta["status"], "warn");
    assert_eq!(
        meta["problems"],
        json!(["event_id", "user_data"]),
        "the pixel runs too, and user data is used"
    );
    let ads = tag("Ads").unwrap();
    assert_eq!(ads["status"], "fail");
    assert_eq!(
        ads["problems"],
        json!(["failed", "transaction_id"]),
        "the click cookie came, so no ad_click warning"
    );
    assert!(
        ads["why"]
            .as_str()
            .unwrap()
            .contains("refresh_token not set")
    );
    assert_eq!(tag("TikTok").unwrap()["problems"], json!(["refused"]));
    assert!(
        tag("Lead ads").is_none(),
        "a tag for another conversion isn't judged on this one"
    );
}
