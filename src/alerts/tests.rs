use std::sync::Mutex;

use axum::Json;
use axum::Router;
use axum::extract::State as AxState;
use axum::routing::post;

use super::*;
use crate::counts::{Runs, Sends};

fn counts(events: &[(&str, u64)]) -> Counts {
    let mut c = Counts {
        hits: events.iter().map(|(_, n)| n).sum(),
        ..Default::default()
    };
    for (e, n) in events {
        c.events.insert(e.to_string(), *n);
    }
    c
}

fn kinds(f: &[Finding]) -> Vec<(&str, &str, &str)> {
    f.iter()
        .map(|f| (f.kind.as_str(), f.subject.as_str(), f.severity.as_str()))
        .collect()
}

#[test]
fn key_events_that_stop_or_drop_are_found_once_there_is_history() {
    let cfg = Config::default();
    let base = vec![counts(&[("purchase", 25), ("add_to_cart", 40)]); 7];
    let now = counts(&[("add_to_cart", 15)]);
    let f = evaluate("", &now, &base, &cfg);
    assert_eq!(
        kinds(&f),
        vec![
            ("event_stopped", "purchase", "problem"),
            ("event_drop", "add_to_cart", "warning")
        ]
    );
    assert_eq!(f[1].numbers["drop_pct"], 62.5);
    let (title, text) = words(f[0].kind, &f[0].subject, &f[0].numbers, "en");
    assert_eq!(title, "purchase stopped");
    assert_eq!(
        text,
        "No purchase in the last hour; usually about 25 at this time."
    );
    assert_eq!(
        words(f[0].kind, &f[0].subject, &f[0].numbers, "tr").0,
        "purchase durdu"
    );

    // Two days of history isn't enough to judge volume.
    assert!(evaluate("", &now, &base[..2], &cfg).is_empty());
    // A quiet event never alarms.
    let quiet = vec![counts(&[("purchase", 3)]); 7];
    assert!(evaluate("", &counts(&[]), &quiet, &cfg).is_empty());
}

#[test]
fn failing_vendors_and_tags_are_found_even_while_learning() {
    let mut now = counts(&[("purchase", 30)]);
    let mut errors = BTreeMap::new();
    errors.insert(
        "400 {\"error\":\"Invalid OAuth access token\"}".to_string(),
        18,
    );
    now.hosts.insert(
        "graph.facebook.com".into(),
        Sends {
            sends: 30,
            failed: 18,
            errors,
        },
    );
    now.hosts.insert(
        "www.google-analytics.com".into(),
        Sends {
            sends: 30,
            failed: 1,
            errors: BTreeMap::new(),
        },
    );
    now.tags.insert(
        "Meta".into(),
        Runs {
            runs: 30,
            failed: 18,
            skipped: 0,
        },
    );
    let f = evaluate("shop", &now, &[], &Config::default());
    let k = kinds(&f);
    assert!(
        k.contains(&("vendor_failing", "graph.facebook.com", "problem")),
        "{k:?}"
    );
    assert!(k.contains(&("tag_failing", "Meta", "problem")), "{k:?}");
    assert!(!k.iter().any(|(_, s, _)| *s == "www.google-analytics.com"));
    assert!(f.iter().all(|f| f.site == "shop"));
    let v = f.iter().find(|f| f.kind == Kind::VendorFailing).unwrap();
    assert!(
        words(v.kind, &v.subject, &v.numbers, "en")
            .1
            .contains("Invalid OAuth")
    );

    // A vendor that always fails this much isn't news.
    let mut usual = now.clone();
    usual.hosts.get_mut("graph.facebook.com").unwrap().failed = 15;
    let f = evaluate("shop", &now, &vec![usual; 3], &Config::default());
    assert!(!kinds(&f).iter().any(|(k, _, _)| *k == "vendor_failing"));
}

#[test]
fn missing_values_unclaimed_hits_and_renamed_events() {
    let mut now = counts(&[("purchase", 20), ("Purchase", 15)]);
    now.missing
        .entry("purchase".into())
        .or_default()
        .insert("currency".into(), 10);
    now.hosts.insert(
        "graph.facebook.com".into(),
        Sends {
            sends: 5,
            failed: 0,
            errors: BTreeMap::new(),
        },
    );
    now.missing
        .entry("purchase".into())
        .or_default()
        .insert("event_id".into(), 20);
    now.hits = 100;
    now.unclaimed = 40;
    let mut base = counts(&[("purchase", 40)]);
    base.hits = 100;
    base.unclaimed = 2;
    let f = evaluate("", &now, &vec![base; 7], &Config::default());
    let k = kinds(&f);
    assert!(
        k.contains(&("missing_value", "purchase.currency", "warning")),
        "{k:?}"
    );
    assert!(
        k.contains(&("missing_value", "purchase.event_id", "warning")),
        "{k:?}"
    );
    assert!(k.contains(&("unclaimed", "", "warning")), "{k:?}");
    assert!(
        k.contains(&("name_changed", "purchase→Purchase", "warning")),
        "{k:?}"
    );
    let n = f.iter().find(|f| f.kind == Kind::NameChanged).unwrap();
    assert_eq!(
        words(n.kind, &n.subject, &n.numbers, "en").0,
        "purchase now arrives as Purchase"
    );
}

#[derive(Clone, Default)]
struct Slack(Arc<Mutex<Vec<String>>>);

async fn fake_slack() -> (String, Slack) {
    let seen = Slack::default();
    async fn hook(AxState(s): AxState<Slack>, Json(v): Json<Value>) -> &'static str {
        s.0.lock()
            .unwrap()
            .push(v["text"].as_str().unwrap_or_default().to_string());
        "ok"
    }
    let app = Router::new()
        .route("/hook", post(hook))
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    (url, seen)
}

fn put(state: &State, q: u64, c: Counts) {
    let mut sites = BySite::new();
    sites.insert(String::new(), c);
    state
        .write(
            &format!("counts/q{q}/i.json"),
            &json!({ "quarter": q, "sites": sites }),
        )
        .unwrap();
}

#[tokio::test]
async fn a_check_opens_posts_and_resolves_alerts() {
    let state = Arc::new(State::memory());
    let history = Arc::new(History::new(state.clone()));
    let (url, slack) = fake_slack().await;
    let alerts = Alerts::new(
        state.clone(),
        history.clone(),
        None,
        Some(url),
        Some("https://x.run.app/admin/".into()),
    );
    let q = quarter_now();
    // The same hour on each of the 7 days before had 10 purchases a quarter.
    for d in 1..=7 {
        for w in 1..=4 {
            put(
                &state,
                q - w - d * QUARTERS_PER_DAY,
                counts(&[("purchase", 10)]),
            );
        }
    }
    history.record(
        "console",
        vec![json!({ "kind": "tag_changed", "name": "Meta", "fields": ["fire_when"] })],
        None,
        None,
    );

    let r = alerts.check(Context { explain: None }).await;
    assert_eq!(r["opened"], 1, "{r}");
    let posted = slack.0.lock().unwrap().clone();
    assert_eq!(posted.len(), 1);
    assert!(
        posted[0].starts_with("🔴 *purchase stopped*"),
        "{}",
        posted[0]
    );
    assert!(posted[0].contains("usually about 40"), "{}", posted[0]);
    assert!(posted[0].contains("Possibly related"), "{}", posted[0]);
    assert!(
        posted[0].contains("changed tag \"Meta\" (fire_when)"),
        "{}",
        posted[0]
    );
    assert!(
        posted[0].contains("<https://x.run.app/admin#alerts|Open the Alerts page>"),
        "{}",
        posted[0]
    );
    assert_eq!(alerts.view()["open"][0]["kind"], "event_stopped");

    // Still broken: no second post.
    alerts.check(Context { explain: None }).await;
    assert_eq!(slack.0.lock().unwrap().len(), 1);

    // Purchases are back in the last hour.
    for w in 1..=4 {
        put(&state, q - w, counts(&[("purchase", 10)]));
    }
    let r = alerts.check(Context { explain: None }).await;
    assert_eq!(r["resolved"], 1, "{r}");
    assert_eq!(
        slack.0.lock().unwrap()[1],
        "✅ *Resolved: purchase stopped*"
    );
    let v = alerts.view();
    assert!(v["open"].as_array().unwrap().is_empty());
    assert_eq!(v["recent"][0]["kind"], "event_stopped");
}

#[tokio::test]
async fn the_ai_explains_new_alerts_and_another_check_waits_for_the_lock() {
    let state = Arc::new(State::memory());
    let alerts = Alerts::new(
        state.clone(),
        Arc::new(History::new(state.clone())),
        None,
        None,
        None,
    );
    let q = quarter_now();
    let mut c = counts(&[("purchase", 30)]);
    c.tags.insert(
        "Meta".into(),
        Runs {
            runs: 30,
            failed: 30,
            skipped: 0,
        },
    );
    put(&state, q - 1, c);
    let explain = |a: Alert| -> futures_util::future::BoxFuture<'static, Option<Value>> {
        Box::pin(
            async move { Some(json!({ "likely_cause": format!("cause of {:?}", a.subject) })) },
        )
    };
    alerts
        .check(Context {
            explain: Some(&explain),
        })
        .await;
    assert_eq!(
        alerts.view()["open"][0]["diagnosis"]["likely_cause"],
        "cause of \"Meta\""
    );

    state
        .write(
            "alerts/lock.json",
            &json!({ "instance": "other", "until": now_ms() + 60_000 }),
        )
        .unwrap();
    assert!(alerts.check(Context { explain: None }).await["skipped"].is_string());
}

#[tokio::test]
async fn an_alert_that_cant_be_judged_stays_open_and_a_worse_one_is_posted_later() {
    let state = Arc::new(State::memory());
    let (url, slack) = fake_slack().await;
    let alerts = Alerts::new(
        state.clone(),
        Arc::new(History::new(state.clone())),
        None,
        Some(url),
        None,
    );
    let q = quarter_now();
    let meta = |runs, failed, skipped| {
        let mut c = counts(&[("purchase", 30)]);
        c.tags.insert(
            "Meta".into(),
            Runs {
                runs,
                failed,
                skipped,
            },
        );
        c
    };
    // 8 of 30 runs failed: a warning.
    put(&state, q - 1, meta(30, 8, 0));
    let r = alerts.check(Context { explain: None }).await;
    assert_eq!(r["opened"], 1, "{r}");
    assert!(slack.0.lock().unwrap()[0].starts_with("🟠"));

    // Within the hour it becomes a problem: not posted yet, but not forgotten.
    put(&state, q - 1, meta(30, 30, 0));
    alerts.check(Context { explain: None }).await;
    assert_eq!(slack.0.lock().unwrap().len(), 1);
    let mut s = state.read("alerts/state.json").unwrap();
    let id = s["open"]
        .as_object()
        .unwrap()
        .keys()
        .next()
        .unwrap()
        .clone();
    assert_eq!(s["open"][&id]["notified_severity"], "warning");
    // An hour after the first post, the next check says it got worse.
    s["open"][&id]["notified_at"] = json!(now_ms() - REPEAT_MS);
    state.write("alerts/state.json", &s).unwrap();
    alerts.check(Context { explain: None }).await;
    let posted = slack.0.lock().unwrap().clone();
    assert_eq!(posted.len(), 2, "{posted:?}");
    assert!(posted[1].starts_with("Worse: 🔴"), "{}", posted[1]);

    // A quiet hour: five runs say nothing either way, so it stays open.
    put(&state, q - 1, meta(5, 0, 0));
    let r = alerts.check(Context { explain: None }).await;
    assert_eq!(
        (r["resolved"].clone(), r["open"].clone()),
        (json!(0), json!(1)),
        "{r}"
    );
    // So do runs that all skipped their event: nothing was tried.
    put(&state, q - 1, meta(30, 0, 30));
    let r = alerts.check(Context { explain: None }).await;
    assert_eq!(r["open"], 1, "{r}");

    // Enough runs, all fine: now it is over.
    put(&state, q - 1, meta(30, 0, 0));
    let r = alerts.check(Context { explain: None }).await;
    assert_eq!(r["resolved"], 1, "{r}");
    assert!(slack.0.lock().unwrap()[2].starts_with("✅"));
}

#[tokio::test]
async fn a_message_slack_didnt_take_goes_out_with_the_next_check() {
    let state = Arc::new(State::memory());
    // Nothing listens here, so the first post fails.
    let down = Alerts::new(
        state.clone(),
        Arc::new(History::new(state.clone())),
        None,
        Some("http://127.0.0.1:9/hook".into()),
        None,
    );
    let q = quarter_now();
    let mut c = counts(&[("purchase", 30)]);
    c.tags.insert(
        "Meta".into(),
        Runs {
            runs: 30,
            failed: 30,
            skipped: 0,
        },
    );
    put(&state, q - 1, c);
    let r = down.check(Context { explain: None }).await;
    assert_eq!(
        (r["opened"].clone(), r["posted"].clone()),
        (json!(1), json!(0)),
        "{r}"
    );
    assert!(r["slack_error"].is_string());

    // Slack is back: the same state, a working address.
    let (url, slack) = fake_slack().await;
    let up = Alerts::new(
        state.clone(),
        Arc::new(History::new(state.clone())),
        None,
        Some(url),
        None,
    );
    let r = up.check(Context { explain: None }).await;
    assert_eq!(r["posted"], 1, "{r}");
    assert!(
        slack.0.lock().unwrap()[0].contains("Meta"),
        "{:?}",
        slack.0.lock().unwrap()
    );
    // And only once.
    up.check(Context { explain: None }).await;
    assert_eq!(slack.0.lock().unwrap().len(), 1);
}

#[test]
fn kept_alerts_keep_their_words_and_survive_one_that_cant_be_read() {
    let state = Arc::new(State::memory());
    let alerts = Alerts::new(
        state.clone(),
        Arc::new(History::new(state.clone())),
        None,
        None,
        None,
    );
    // As an earlier version kept them: no `notified_severity`, and one of a
    // kind this version doesn't have.
    let old = |kind: &str, severity: &str| {
        json!({
            "id": format!("{kind}||Meta"), "kind": kind, "site": "", "subject": "Meta",
            "severity": severity, "numbers": {}, "texts": {},
            "started_at": 5, "updated_at": 5, "notified_at": 5, "related": [], "diagnosis": null,
        })
    };
    state
        .write(
            "alerts/state.json",
            &json!({
                "open": {
                    "tag_failing||Meta": old("tag_failing", "warning"),
                    "long_gone||Meta": old("long_gone", "problem"),
                },
                "recent": [old("event_drop", "problem")],
                "last_check": { "at": 5 },
            }),
        )
        .unwrap();
    let v = alerts.view();
    let open = v["open"].as_array().unwrap();
    assert_eq!(open.len(), 1, "{v}");
    // The console reads these words.
    assert_eq!(open[0]["kind"], "tag_failing");
    assert_eq!(open[0]["severity"], "warning");
    assert_eq!(v["recent"][0]["kind"], "event_drop");
    assert_eq!(v["last_check"]["at"], 5);
    for kind in [
        Kind::EventStopped,
        Kind::EventDrop,
        Kind::VendorFailing,
        Kind::TagFailing,
        Kind::MissingValue,
        Kind::Unclaimed,
        Kind::NameChanged,
    ] {
        assert_eq!(json!(kind), kind.as_str());
        // Every kind has its own words in both languages.
        for lang in ["en", "tr"] {
            let (title, text) = words(kind, "purchase.value", &json!({}), lang);
            assert!(!title.is_empty() && !text.is_empty(), "{kind:?} {lang}");
        }
    }
    assert!(Severity::Problem > Severity::Warning);
}

#[test]
fn config_is_cleaned_and_checked() {
    let state = Arc::new(State::memory());
    let a = Alerts::new(
        state.clone(),
        Arc::new(History::new(state)),
        None,
        None,
        None,
    );
    assert_eq!(a.config(), Config::default());
    let c = a
        .set_config(Config {
            key_events: vec![" lead ".into(), "lead".into(), "".into()],
            language: "tr".into(),
        })
        .unwrap();
    assert_eq!(c.key_events, vec!["lead"]);
    assert_eq!(a.config().language, "tr");
    assert!(
        a.set_config(Config {
            key_events: vec![],
            language: "de".into()
        })
        .is_err()
    );
}
