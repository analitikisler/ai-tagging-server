//! The setup check: whether the website, this server and the ad platforms
//! are connected and complete. It reads what the server already has (the
//! last 24 hours of counts, recent hits and the container that runs) and,
//! when a site is configured, the site's page and its web GTM container,
//! and lists each finding with the side it must be fixed on: the website
//! (or its web container) or this server.
//!
//! Findings are `{ kind, status, side, params }`; the console words them,
//! and the AI reads them for its to-do list.

use std::collections::BTreeSet;
use std::time::Duration;

use regex::Regex;
use serde_json::{Value, json};

use crate::ai::setup::needs;
use crate::counts::Counts;
use crate::integrations::PROVIDERS;

/// Most bytes read from a page or a GTM container.
const MAX_FETCH: usize = 5 * 1024 * 1024;

fn item(kind: &str, status: &str, side: &str, params: Value) -> Value {
    json!({ "kind": kind, "status": status, "side": side, "params": params })
}

fn pct(a: u64, b: u64) -> f64 {
    if b == 0 {
        0.0
    } else {
        (a as f64 * 1000.0 / b as f64).round() / 10.0
    }
}

/// The event a tag sends as a conversion.
fn conversion_event(tag: &Value) -> String {
    match tag["template"].as_str().unwrap_or_default() {
        "google_ads" => tag["data"]["conversion_event"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("purchase")
            .to_string(),
        _ => "purchase".into(),
    }
}

fn truthy(v: &Value) -> bool {
    v == &json!(true) || v == &json!("true")
}

/// Whether a tag is still in its platform's test mode.
fn in_test_mode(tag: &Value) -> bool {
    let d = &tag["data"];
    match tag["template"].as_str().unwrap_or_default() {
        "meta_capi" | "tiktok_events" => {
            d["test_event_code"].as_str().is_some_and(|s| !s.is_empty())
        }
        "pinterest_capi" | "google_ads" => truthy(&d["test_mode"]),
        _ => false,
    }
}

/// The platform a browser pixel would duplicate, for a tag template.
fn pixel_of(template: &str) -> Option<&'static str> {
    match template {
        "meta_capi" => Some("meta"),
        "tiktok_events" => Some("tiktok"),
        "pinterest_capi" => Some("pinterest"),
        "google_ads" => Some("google_ads"),
        _ => None,
    }
}

/// The findings. `counts` covers the last 24 hours for the site, `hits` are
/// recent traces (their details), and `web` is [`analyse`]'s result when a
/// site is configured.
pub fn check(
    container: &Value,
    counts: &Counts,
    hits: &[Value],
    web: Option<&Value>,
) -> Vec<Value> {
    let mut out = Vec::new();
    let events: Vec<&Value> = hits
        .iter()
        .flat_map(|t| t["result"]["events"].as_array().into_iter().flatten())
        .collect();

    // The website reaching this server.
    if counts.hits == 0 {
        out.push(item("no_hits", "bad", "website", json!({})));
    } else {
        out.push(item(
            "hits",
            "ok",
            "website",
            json!({ "hits": counts.hits }),
        ));
        if counts.unclaimed >= 5 && pct(counts.unclaimed, counts.hits) >= 10.0 {
            out.push(item("unclaimed", "warn", "both", json!({ "unclaimed": counts.unclaimed, "hits": counts.hits, "pct": pct(counts.unclaimed, counts.hits) })));
        }
    }
    if events.len() >= 5 {
        let with = events.iter().filter(|e| e["x-ga-gcs"].is_string()).count() as u64;
        let p = pct(with, events.len() as u64);
        if p >= 80.0 {
            out.push(item("consent", "ok", "website", json!({ "pct": p })));
        } else {
            out.push(item(
                "consent_missing",
                "warn",
                "website",
                json!({ "pct": p }),
            ));
        }
    }
    if hits
        .iter()
        .any(|t| t["result"]["claimed_by"] == "Google scripts")
    {
        out.push(item("scripts_first_party", "ok", "website", json!({})));
    }

    // The tags on this server.
    let tags: Vec<Value> = container["tags"].as_array().cloned().unwrap_or_default();
    let enabled: Vec<&Value> = tags
        .iter()
        .filter(|t| t["enabled"] != json!(false))
        .collect();
    let missing = needs(&enabled.iter().map(|t| (*t).clone()).collect::<Vec<_>>());
    let mut by_tag: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for n in &missing {
        by_tag
            .entry(n["tag"].as_str().unwrap_or_default().to_string())
            .or_default()
            .push(n["label"].as_str().unwrap_or_default().to_string());
    }
    for (tag, labels) in &by_tag {
        out.push(item(
            "tag_settings",
            "bad",
            "server",
            json!({ "tag": tag, "settings": labels }),
        ));
    }
    for t in &enabled {
        let name = t["name"].as_str().unwrap_or_default();
        if in_test_mode(t) {
            out.push(item("test_mode", "warn", "server", json!({ "tag": name })));
        }
        if by_tag.contains_key(name) {
            continue;
        }
        match counts.tags.get(name) {
            Some(r) if r.runs > 0 && pct(r.failed, r.runs) >= 10.0 => {
                // The first failing run's log line says why.
                let why = hits.iter().find_map(|h| {
                    let failed = h["result"]["tags"]
                        .as_array()?
                        .iter()
                        .any(|x| x["tag"] == name && x["outcome"] != "success");
                    if !failed {
                        return None;
                    }
                    let prefix = format!("[{name}]");
                    h["result"]["logs"]
                        .as_array()?
                        .iter()
                        .filter_map(Value::as_str)
                        .find(|l| l.starts_with(&prefix))
                        .map(|l| l.chars().take(200).collect::<String>())
                });
                out.push(item("tag_failing", "bad", "server", json!({ "tag": name, "failed": r.failed, "runs": r.runs, "pct": pct(r.failed, r.runs), "why": why })));
            }
            Some(r) if r.runs > 0 => out.push(item(
                "tag_ok",
                "ok",
                "server",
                json!({ "tag": name, "runs": r.runs }),
            )),
            _ if counts.hits > 0 => {
                out.push(item("tag_idle", "warn", "both", json!({ "tag": name })))
            }
            _ => {}
        }
    }
    let ad_tags: Vec<&&Value> = enabled
        .iter()
        .filter(|t| PROVIDERS.iter().any(|p| t["template"] == p.template))
        .collect();
    if ad_tags.is_empty() {
        out.push(item("no_ad_platforms", "info", "server", json!({})));
    }

    // What the website sends, against what the tags need.
    let pixels: BTreeSet<String> = web
        .and_then(|w| w["pixels"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|p| p.as_str().map(str::to_string))
        .collect();
    let mut said: BTreeSet<String> = BTreeSet::new();
    let mut once = |key: String, it: Value, out: &mut Vec<Value>| {
        if said.insert(key) {
            out.push(it);
        }
    };
    for t in &ad_tags {
        let name = t["name"].as_str().unwrap_or_default();
        let template = t["template"].as_str().unwrap_or_default();
        let ev = conversion_event(t);
        let n = counts.events.get(&ev).copied().unwrap_or(0);
        if counts.hits > 0 && n == 0 {
            out.push(item(
                "conversion_missing",
                "warn",
                "website",
                json!({ "tag": name, "event": ev }),
            ));
            continue;
        }
        if n == 0 {
            continue;
        }
        let miss = |k: &str| {
            counts
                .missing
                .get(&ev)
                .and_then(|m| m.get(k))
                .copied()
                .unwrap_or(0)
        };
        for k in ["value", "currency"] {
            if pct(miss(k), n) >= 20.0 {
                once(
                    format!("{ev}.{k}"),
                    item(
                        "value_missing",
                        "warn",
                        "website",
                        json!({ "event": ev, "key": k, "pct": pct(miss(k), n) }),
                    ),
                    &mut out,
                );
            }
        }
        if template == "google_ads" && pct(miss("transaction_id"), n) >= 20.0 {
            once(
                format!("{ev}.transaction_id"),
                item(
                    "value_missing",
                    "warn",
                    "website",
                    json!({ "event": ev, "key": "transaction_id", "pct": pct(miss("transaction_id"), n) }),
                ),
                &mut out,
            );
        }
        if matches!(template, "meta_capi" | "tiktok_events") && pct(miss("event_id"), n) >= 20.0 {
            let pixel = pixel_of(template).is_some_and(|p| pixels.contains(p));
            out.push(item("event_id_missing", if pixel { "warn" } else { "info" }, "website", json!({ "tag": name, "event": ev, "pct": pct(miss("event_id"), n), "pixel": pixel })));
        }
        let conv: Vec<&&Value> = events
            .iter()
            .filter(|e| e["event_name"] == json!(ev))
            .collect();
        let with_user = conv.iter().filter(|e| e["user_data"].is_object()).count();
        let uses = truthy(&t["data"]["eventUserData"]);
        if uses && !conv.is_empty() && with_user == 0 {
            out.push(item(
                "user_data_missing",
                "warn",
                "website",
                json!({ "tag": name, "event": ev }),
            ));
        } else if !uses && with_user > 0 {
            out.push(item(
                "user_data_unused",
                "info",
                "server",
                json!({ "tag": name }),
            ));
        }
        if template == "google_ads" {
            let clicks = events
                .iter()
                .filter(|e| {
                    e["page_location"].as_str().is_some_and(|u| {
                        ["gclid=", "gbraid=", "wbraid="]
                            .iter()
                            .any(|k| u.contains(k))
                    })
                })
                .count();
            out.push(if clicks > 0 {
                item(
                    "ad_clicks",
                    "ok",
                    "website",
                    json!({ "tag": name, "clicks": clicks }),
                )
            } else {
                item(
                    "ad_clicks_unseen",
                    "info",
                    "website",
                    json!({ "tag": name }),
                )
            });
        }
    }

    // The website's own page and web container.
    if let Some(w) = web {
        out.extend(web_items(w, &events));
    }
    out
}

/// Words ad blocker filter lists match in hostnames and paths.
const BLOCKED_WORDS: &[&str] = &[
    "ad",
    "ads",
    "analytics",
    "collect",
    "gtag",
    "gtm",
    "metrics",
    "pixel",
    "sgtm",
    "stape",
    "stats",
    "tag",
    "tagging",
    "tags",
    "telemetry",
    "track",
    "tracker",
    "tracking",
];

fn words(s: &str) -> impl Iterator<Item = String> + '_ {
    s.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_lowercase)
}

/// The first blocked word in `url`'s host or path, leaving out the words of
/// the site's own host (a shop called adhouse.com can't avoid "ad").
fn blocked_word(url: &str, site: &BTreeSet<String>) -> Option<&'static str> {
    let u = reqwest::Url::parse(url).ok()?;
    let host = u.host_str().unwrap_or_default().to_lowercase();
    words(&host)
        .chain(words(u.path()))
        .filter(|w| !site.contains(w))
        .find_map(|w| BLOCKED_WORDS.iter().find(|b| **b == w).copied())
}

fn web_items(w: &Value, events: &[&Value]) -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(e) = w["site_error"].as_str() {
        out.push(item(
            "site_unreachable",
            "bad",
            "website",
            json!({ "url": w["site"], "error": e }),
        ));
        return out;
    }
    let gtm = w["gtm_id"].as_str().unwrap_or_default();
    let direct = w["server_urls"].as_array().is_some_and(|u| !u.is_empty());
    // A page that runs gtag.js itself and sends to a server needs no GTM.
    if gtm.is_empty() && !direct {
        out.push(item(
            "no_web_container",
            "warn",
            "website",
            json!({ "url": w["site"] }),
        ));
    } else if let Some(e) = w["gtm_error"].as_str() {
        out.push(item(
            "web_container_unreadable",
            "warn",
            "website",
            json!({ "gtm": gtm, "error": e }),
        ));
    }
    let urls: Vec<String> = w["server_urls"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|u| u.as_str().map(str::to_string))
        .collect();
    let public = w["public_url"]
        .as_str()
        .unwrap_or_default()
        .trim_end_matches('/')
        .to_lowercase();
    let here = urls
        .iter()
        .any(|u| !public.is_empty() && u.trim_end_matches('/').to_lowercase().starts_with(&public));
    if urls.is_empty() {
        out.push(item(
            "server_url_missing",
            "bad",
            "website",
            json!({ "gtm": gtm, "public_url": public }),
        ));
    } else if public.is_empty() || here {
        out.push(item(
            "server_url",
            if here { "ok" } else { "info" },
            "website",
            json!({ "urls": urls }),
        ));
    } else {
        out.push(item(
            "server_url_elsewhere",
            "bad",
            "website",
            json!({ "urls": urls, "public_url": public }),
        ));
    }
    // A server address with a word blockers look for gets blocked sooner.
    let site_labels: BTreeSet<String> = w["site"]
        .as_str()
        .and_then(|s| reqwest::Url::parse(s).ok())
        .and_then(|u| u.host_str().map(str::to_lowercase))
        .map(|h| words(&h).collect())
        .unwrap_or_default();
    for u in &urls {
        if let Some(word) = blocked_word(u, &site_labels) {
            out.push(item(
                "server_url_word",
                "warn",
                "website",
                json!({ "url": u, "word": word }),
            ));
        }
    }
    // GA4 ids the web side configures against the ones arriving here.
    let web_ids: BTreeSet<String> = w["ga4_ids"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| i.as_str().map(str::to_string))
        .collect();
    let hit_ids: BTreeSet<String> = events
        .iter()
        .filter_map(|e| e["x-ga-measurement_id"].as_str().map(str::to_string))
        .collect();
    if !web_ids.is_empty() && !hit_ids.is_empty() && web_ids.is_disjoint(&hit_ids) {
        out.push(item(
            "ga4_ids_differ",
            "warn",
            "both",
            json!({ "web": web_ids, "hits": hit_ids }),
        ));
    }
    if w["first_party_gtm"] == json!(true) {
        out.push(item("gtm_first_party", "ok", "website", json!({})));
    }
    let pixels: Vec<&Value> = w["pixels"].as_array().into_iter().flatten().collect();
    if !pixels.is_empty() {
        out.push(item(
            "browser_pixels",
            "info",
            "website",
            json!({ "pixels": pixels }),
        ));
    }
    out
}

/// What a page or GTM container text shows: GTM and GA4 ids, the server
/// URLs a Google tag sends to, and browser pixels.
pub fn scan(text: &str) -> Value {
    let found = |pattern: &str| -> Vec<String> {
        let re = Regex::new(pattern).expect("pattern");
        let set: BTreeSet<String> = re.find_iter(text).map(|m| m.as_str().to_string()).collect();
        set.into_iter().collect()
    };
    let url = Regex::new(r#"https?:(?:\\?/){2}[A-Za-z0-9.\-]+(?::\d+)?(?:\\?/[^"'\s\\,)}]*)*"#)
        .expect("url");
    let mut urls = BTreeSet::new();
    for key in ["server_container_url", "transport_url"] {
        for (at, _) in text.match_indices(key) {
            let end = (at + 400).min(text.len());
            let mut end = end;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            if let Some(m) = url.find(&text[at..end]) {
                urls.insert(m.as_str().replace("\\/", "/"));
            }
        }
    }
    let has = |needles: &[&str]| needles.iter().any(|n| text.contains(n));
    let mut pixels = Vec::new();
    if has(&["connect.facebook.net", "fbq("]) {
        pixels.push("meta");
    }
    if has(&["analytics.tiktok.com", "ttq.load", "ttq.track"]) {
        pixels.push("tiktok");
    }
    if has(&["s.pinimg.com/ct", "pintrk("]) {
        pixels.push("pinterest");
    }
    if !found(r"\bAW-\d{6,12}\b").is_empty() {
        pixels.push("google_ads");
    }
    json!({
        "gtm_ids": found(r"\bGTM-[A-Z0-9]{4,10}\b"),
        "ga4_ids": found(r"\bG-[A-Z0-9]{6,12}\b"),
        "server_urls": urls,
        "pixels": pixels,
    })
}

/// Whether a URL names a private or internal address outright; names are
/// checked by the client's resolver, which IP literals never reach.
fn private_literal(url: &reqwest::Url) -> bool {
    url.host_str()
        .and_then(|h| h.trim_matches(['[', ']']).parse::<std::net::IpAddr>().ok())
        .is_some_and(crate::server::is_private)
}

/// Reads a public URL's text, following up to 5 redirects. The client
/// refuses private and internal addresses.
async fn fetch(client: &(reqwest::Client, bool), url: &str) -> Result<String, String> {
    let (client, allow_private) = client;
    let parsed = reqwest::Url::parse(url).map_err(|e| e.to_string())?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("only http and https addresses can be checked".into());
    }
    if !allow_private && private_literal(&parsed) {
        return Err("destination is private or internal".into());
    }
    let r = client.get(parsed).send().await.map_err(|e| e.to_string())?;
    let status = r.status();
    let bytes = r.bytes().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
    }
    let cut = &bytes[..bytes.len().min(MAX_FETCH)];
    Ok(String::from_utf8_lossy(cut).into_owned())
}

/// A client for the site and GTM: public addresses only (each redirect is
/// checked too), at most 5 redirects.
pub fn client(allow_private: bool) -> (reqwest::Client, bool) {
    let policy = reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= 5 {
            attempt.error("too many redirects")
        } else if !allow_private && private_literal(attempt.url()) {
            attempt.error("redirected to a private or internal address")
        } else {
            attempt.follow()
        }
    });
    let mut b = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(policy)
        .user_agent("Mozilla/5.0 (compatible; AI-Tagging-Server setup check)");
    if !allow_private {
        b = b.dns_resolver(std::sync::Arc::new(crate::server::PublicOnlyResolver));
    }
    (b.build().expect("http client"), allow_private)
}

/// Reads the site's page and its web GTM container (`gtm_id`, or the first
/// one the page loads), and sums up what they show. `public_url` is this
/// server's address as the site reaches it, e.g. https://example.com/k7q2.
pub async fn analyse(
    client: &(reqwest::Client, bool),
    site: &str,
    gtm_id: Option<&str>,
    public_url: Option<&str>,
) -> Value {
    analyse_from(
        client,
        site,
        gtm_id,
        public_url,
        "https://www.googletagmanager.com",
    )
    .await
}

/// [`analyse`], with GTM containers read from `gtm_base`.
pub async fn analyse_from(
    client: &(reqwest::Client, bool),
    site: &str,
    gtm_id: Option<&str>,
    public_url: Option<&str>,
    gtm_base: &str,
) -> Value {
    let mut out = json!({ "site": site, "public_url": public_url, "at": crate::state::now_ms() });
    let page = match fetch(client, site).await {
        Ok(p) => p,
        Err(e) => {
            out["site_error"] = json!(e);
            return out;
        }
    };
    let on_page = scan(&page);
    let gtm = gtm_id
        .map(str::to_string)
        .filter(|g| !g.is_empty())
        .or_else(|| on_page["gtm_ids"][0].as_str().map(str::to_string));
    // A page that loads gtm.js from its own domain goes through this server.
    let first_party = gtm.as_ref().is_some_and(|g| {
        page.contains(&format!("/gtm.js?id={g}"))
            && !page.contains(&format!("googletagmanager.com/gtm.js?id={g}"))
    });
    let mut merged = on_page.clone();
    if let Some(g) = &gtm {
        out["gtm_id"] = json!(g);
        match fetch(client, &format!("{gtm_base}/gtm.js?id={g}")).await {
            Ok(js) => {
                let c = scan(&js);
                for k in ["ga4_ids", "server_urls", "pixels"] {
                    let mut all: BTreeSet<String> = BTreeSet::new();
                    for v in [&merged[k], &c[k]] {
                        all.extend(
                            v.as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(|x| x.as_str().map(str::to_string)),
                        );
                    }
                    merged[k] = json!(all);
                }
            }
            Err(e) => out["gtm_error"] = json!(e),
        }
    }
    for k in ["ga4_ids", "server_urls", "pixels"] {
        out[k] = merged[k].clone();
    }
    out["first_party_gtm"] = json!(first_party);
    out
}

/// A live check: the hits of one browser that opened the site with
/// `?sst_check=<code>`. The code marks the first page; the GA4 client id it
/// carries follows the browser to later pages. `hits` are trace details,
/// with their ids, newest first; only those since `since` count.
/// How each tag handled the session's latest conversion: `pass`, `warn`
/// (sent, but without something it needs to count or match well) or `fail`
/// (didn't run, failed, sent nothing, or the platform refused), with the
/// problems and the tag's log line. Waiting, with the events that count,
/// until the session sends one.
fn verdict(session: &[(u64, &Value)], container: &Value, pixels: &BTreeSet<String>) -> Value {
    let tags: Vec<&Value> = container["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| t["enabled"] != json!(false))
        .collect();
    let is_ad = |t: &Value| PROVIDERS.iter().any(|p| t["template"] == p.template);
    let mut conversions: BTreeSet<String> = tags
        .iter()
        .filter(|t| is_ad(t))
        .map(|t| conversion_event(t))
        .collect();
    if conversions.is_empty() {
        conversions.insert("purchase".into());
    }
    let click_in = |t: &Value| {
        let urls = t["result"]["events"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| e["page_location"].as_str())
            .any(|u| {
                ["gclid=", "gbraid=", "wbraid="]
                    .iter()
                    .any(|k| u.contains(k))
            });
        // Traces keep cookie names: the click cookie is there when it was set.
        let cookie = t["request"]["headers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|h| h[0] == "cookie")
            .filter_map(|h| h[1].as_str())
            .any(|c| {
                ["_sst_ads", "_gcl_aw", "_gcl_gb"]
                    .iter()
                    .any(|n| c.contains(n))
            });
        urls || cookie
    };
    let clicked = session.iter().any(|(_, t)| click_in(t));
    let found = session.iter().rev().find_map(|(id, t)| {
        let evs = t["result"]["events"].as_array()?;
        let (i, e) = evs.iter().enumerate().rev().find(|(_, e)| {
            e["event_name"]
                .as_str()
                .is_some_and(|n| conversions.contains(n))
        })?;
        Some((*id, t, i, e))
    });
    let Some((id, t, idx, ev)) = found else {
        return json!({ "waiting": conversions });
    };
    let r = &t["result"];
    let name_of = ev["event_name"].as_str().unwrap_or_default().to_string();
    let present = |k: &str| match &ev[k] {
        Value::Null => false,
        Value::String(s) => !s.trim().is_empty(),
        _ => true,
    };
    let mut results = Vec::new();
    for tag in &tags {
        let name = tag["name"].as_str().unwrap_or_default();
        let template = tag["template"].as_str().unwrap_or_default();
        let should = if is_ad(tag) {
            conversion_event(tag) == name_of
        } else {
            template == "ga4_tag"
        };
        let run = r["tags"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|x| x["tag"] == name && x["event"].as_u64() == Some(idx as u64));
        if run.is_none() && !should {
            continue;
        }
        let sends: Vec<&Value> = r["sends"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|s| s["from"] == name)
            .collect();
        let refused = sends
            .iter()
            .any(|s| s["result"]["Ok"].as_u64().is_none_or(|c| c >= 400));
        let prefix = format!("[{name}]");
        let why = r["logs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .find(|l| l.starts_with(&prefix))
            .map(|l| l.chars().take(200).collect::<String>());
        let mut fails: Vec<&str> = Vec::new();
        match run {
            None => fails.push("not_run"),
            Some(x) if x["outcome"] != "success" => fails.push("failed"),
            _ if sends.is_empty() => fails.push("not_sent"),
            _ if refused => fails.push("refused"),
            _ => {}
        }
        let mut warns: Vec<&str> = Vec::new();
        if is_ad(tag) {
            for k in ["value", "currency"] {
                if !present(k) {
                    warns.push(k);
                }
            }
            if template == "google_ads" {
                if !present("transaction_id") {
                    warns.push("transaction_id");
                }
                if !clicked {
                    warns.push("ad_click");
                }
            }
            if matches!(template, "meta_capi" | "tiktok_events")
                && !present("event_id")
                && pixel_of(template).is_some_and(|p| pixels.contains(p))
            {
                warns.push("event_id");
            }
            if truthy(&tag["data"]["eventUserData"]) && !ev["user_data"].is_object() {
                warns.push("user_data");
            }
        }
        let status = if !fails.is_empty() {
            "fail"
        } else if !warns.is_empty() {
            "warn"
        } else {
            "pass"
        };
        fails.extend(warns);
        results.push(json!({ "tag": name, "template": template, "status": status, "problems": fails, "why": why }));
    }
    json!({ "event": name_of, "trace": id, "at": t["at"], "tags": results })
}

pub fn live(
    code: &str,
    since: u64,
    hits: &[(u64, Value)],
    container: &Value,
    pixels: &BTreeSet<String>,
) -> Value {
    let marker = format!("sst_check={code}");
    let recent: Vec<&(u64, Value)> = hits
        .iter()
        .filter(|(_, t)| t["at"].as_u64().unwrap_or(0) >= since)
        .collect();
    let clients: BTreeSet<String> = recent
        .iter()
        .flat_map(|(_, t)| t["result"]["events"].as_array().into_iter().flatten())
        .filter(|e| {
            e["page_location"]
                .as_str()
                .is_some_and(|u| u.contains(&marker))
        })
        .filter_map(|e| e["client_id"].as_str().map(str::to_string))
        .collect();
    let mut out = Vec::new();
    let mut session: Vec<(u64, &Value)> = Vec::new();
    for (id, t) in recent.iter().rev() {
        let r = &t["result"];
        let events: Vec<&Value> = r["events"].as_array().into_iter().flatten().collect();
        let mine = events.iter().any(|e| {
            e["client_id"].as_str().is_some_and(|c| clients.contains(c))
                || e["page_location"]
                    .as_str()
                    .is_some_and(|u| u.contains(&marker))
        });
        if !mine {
            continue;
        }
        session.push((*id, t));
        let evs: Vec<Value> = events
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let has = |k: &str| match &e[k] {
                    Value::Null => false,
                    Value::String(s) => !s.trim().is_empty(),
                    _ => true,
                };
                let tags: Vec<Value> = r["tags"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|x| x["event"].as_u64() == Some(i as u64))
                    .map(|x| json!({ "tag": x["tag"], "outcome": x["outcome"] }))
                    .collect();
                let url = e["page_location"].as_str().unwrap_or_default();
                let ad_click = ["gclid=", "gbraid=", "wbraid="].iter().any(|k| url.contains(k));
                json!({
                    "name": e["event_name"],
                    "page": url,
                    "has": {
                        "value": has("value"), "currency": has("currency"), "transaction_id": has("transaction_id"),
                        "event_id": has("event_id"), "user_data": e["user_data"].is_object(), "consent": has("x-ga-gcs"),
                        "ad_click": ad_click,
                    },
                    "tags": tags,
                })
            })
            .collect();
        let sends = r["sends"].as_array().map_or(0, Vec::len);
        let failed = r["sends"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|s| s["result"]["Ok"].as_u64().is_none_or(|c| c >= 400))
            .count();
        out.push(json!({ "trace": id, "at": t["at"], "events": evs, "sends": sends, "sends_failed": failed }));
    }
    let verdict = if session.is_empty() {
        Value::Null
    } else {
        verdict(&session, container, pixels)
    };
    json!({ "code": code, "since": since, "found": !clients.is_empty() || !out.is_empty(), "hits": out, "verdict": verdict })
}

#[cfg(test)]
mod tests;
