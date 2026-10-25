//! GA4 Measurement Protocol v2 (`/g/collect`) parsing, behind the
//! `isRequestMpv2` and `extractEventsFromMpv2` sandbox APIs.
//!
//! Events follow Google sGTM's common event data shape. The mapping covers
//! the parameters gtag.js sends for page views, custom events and ecommerce
//! items (`pr1`, `pr2`, … become `items`).
//!
//! User-provided data becomes sGTM's `user_data` object (`email_address`,
//! `phone_number`, `sha256_email_address`, `address.first_name` and so on).
//! It arrives as `ep.user_data.*` parameters (the tag's user data, plain or
//! `sha256_` keys) or as the Google tag's `em` envelope
//! (`tv.1~em.<hash>~ph.<hash>…`) when the property collects user-provided
//! data. Digests sent as base64 are turned into hex, the form ad platforms
//! take.

use std::collections::BTreeMap;

use base64::Engine as _;
use serde_json::{Map, Number, Value};

use crate::request::IncomingRequest;

/// Key holding the event's own encoded parameters, so a tag can forward the
/// hit exactly as the browser sent it.
pub const RAW_HIT_KEY: &str = "x-sst-raw-hit";

const COLLECT_PATHS: [&str; 3] = ["/g/collect", "/mp/collect", "/j/collect"];

pub fn is_mpv2(req: &IncomingRequest) -> bool {
    COLLECT_PATHS.contains(&req.path.as_str())
        && parse(&req.query).iter().any(|(k, v)| k == "v" && v == "2")
}

/// One event per body line (batched `POST`), or one for the query alone.
/// Line parameters override the shared query parameters.
pub fn extract_events(req: &IncomingRequest) -> Vec<Value> {
    let shared = parse(&req.query);
    let lines: Vec<&str> = req
        .body
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();

    let hits: Vec<Vec<(String, String)>> = if lines.is_empty() {
        vec![shared]
    } else {
        lines
            .iter()
            .map(|line| merge(&shared, &parse(line)))
            .collect()
    };

    hits.into_iter()
        .map(|params| to_event(&params, req))
        .collect()
}

fn parse(s: &str) -> Vec<(String, String)> {
    form_urlencoded::parse(s.trim_start_matches('?').as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

fn merge(shared: &[(String, String)], line: &[(String, String)]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = shared
        .iter()
        .filter(|(k, _)| !line.iter().any(|(lk, _)| lk == k))
        .cloned()
        .collect();
    out.extend(line.iter().cloned());
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

/// A SHA-256 digest as lower-case hex, from hex or (url-safe) base64.
fn hex_digest(v: &str) -> Option<String> {
    let v = v.trim();
    if v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Some(v.to_ascii_lowercase());
    }
    let b64: String = v
        .trim_end_matches('=')
        .chars()
        .map(|c| match c {
            '+' | ' ' => '-',
            '/' => '_',
            c => c,
        })
        .collect();
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(b64)
        .ok()?;
    (bytes.len() == 32).then(|| bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Sets `path` inside `map`, making the objects on the way.
fn put(map: &mut Map<String, Value>, path: &[&str], v: Value) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut m = map;
    for p in parents {
        let next = m
            .entry(p.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !next.is_object() {
            *next = Value::Object(Map::new());
        }
        m = next.as_object_mut().expect("object");
    }
    m.insert(last.to_string(), v);
}

/// Event keys starting with `x-` are the server's own (consent, the raw hit,
/// visitor info, a server source's mark); a hit's parameters can't set them.
fn reserved(name: &str) -> bool {
    name.starts_with("x-")
}

/// One `ep.user_data.<path>` parameter. `email` and `phone` are taken as
/// sGTM's `email_address` and `phone_number`; a `sha256_` value is kept only
/// as a digest.
fn user_param(user: &mut Map<String, Value>, path: &str, v: Value) {
    let mut parts: Vec<&str> = path.split('.').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() || parts.len() > 3 {
        return;
    }
    match parts[0] {
        "email" => parts[0] = "email_address",
        "phone" => parts[0] = "phone_number",
        _ => {}
    }
    let last = parts[parts.len() - 1];
    let v = if last.starts_with("sha256_") {
        match v.as_str().and_then(hex_digest) {
            Some(h) => Value::String(h),
            None => return,
        }
    } else {
        v
    };
    put(user, &parts, v);
}

/// The Google tag's `em` envelope: `tv.1~em.<v>~ph.<v>~fn.<v>~ln.<v>~sa.<v>~ct.<v>~rg.<v>~pc.<v>~co.<v>`.
/// Digests go to the `sha256_` keys, anything else to the plain ones.
fn user_envelope(user: &mut Map<String, Value>, em: &str) {
    for part in em.split('~') {
        let Some((code, v)) = part.split_once('.') else {
            continue;
        };
        if v.is_empty() || code == "tv" {
            continue;
        }
        let (plain, hashed): (&[&str], &[&str]) = match code {
            "em" => (&["email_address"], &["sha256_email_address"]),
            "ph" => (&["phone_number"], &["sha256_phone_number"]),
            "fn" => (
                &["address", "first_name"],
                &["address", "sha256_first_name"],
            ),
            "ln" => (&["address", "last_name"], &["address", "sha256_last_name"]),
            "sa" => (&["address", "street"], &["address", "sha256_street"]),
            "ct" => (&["address", "city"], &["address", "sha256_city"]),
            "rg" => (&["address", "region"], &["address", "sha256_region"]),
            "pc" => (
                &["address", "postal_code"],
                &["address", "sha256_postal_code"],
            ),
            "co" => (&["address", "country"], &["address", "sha256_country"]),
            _ => continue,
        };
        match hex_digest(v) {
            Some(h) => put(user, hashed, Value::String(h)),
            None => put(user, plain, Value::String(v.to_string())),
        }
    }
}

/// Lays `over` on top of `base`, object by object.
fn merge_into(base: &mut Map<String, Value>, over: Map<String, Value>) {
    for (k, v) in over {
        match (base.get_mut(&k), v) {
            (Some(Value::Object(b)), Value::Object(o)) => merge_into(b, o),
            (_, v) => {
                base.insert(k, v);
            }
        }
    }
}

/// Item field names behind gtag.js's two-letter prefixes, and whether the
/// value is a number. `k0`…`k9` / `v0`…`v9` carry custom item parameters.
fn item_field(prefix: &str) -> Option<(&'static str, bool)> {
    Some(match prefix {
        "id" => ("item_id", false),
        "nm" => ("item_name", false),
        "br" => ("item_brand", false),
        "ca" => ("item_category", false),
        "c2" => ("item_category2", false),
        "c3" => ("item_category3", false),
        "c4" => ("item_category4", false),
        "c5" => ("item_category5", false),
        "va" => ("item_variant", false),
        "pr" => ("price", true),
        "qt" => ("quantity", true),
        "cp" => ("coupon", false),
        "ds" => ("discount", true),
        "af" => ("affiliation", false),
        "lp" => ("index", true),
        "ln" => ("item_list_name", false),
        "li" => ("item_list_id", false),
        "lo" => ("location_id", false),
        "pi" => ("promotion_id", false),
        "pn" => ("promotion_name", false),
        "cn" => ("creative_name", false),
        "cs" => ("creative_slot", false),
        _ => return None,
    })
}

/// One `prN` value, such as `idSKU1~nmShirt~pr19.9~qt2~k0size~v0M`, as a
/// GA4 item object.
fn to_item(v: &str) -> Map<String, Value> {
    let mut item = Map::new();
    let mut custom_keys = BTreeMap::new();
    let mut custom_values = BTreeMap::new();
    for part in v.split('~') {
        let Some((prefix, value)) = part.split_at_checked(2) else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        let mut chars = prefix.chars();
        match (chars.next(), chars.next()) {
            (Some('k'), Some(d)) if d.is_ascii_digit() => {
                custom_keys.insert(d, value.to_string());
            }
            (Some('v'), Some(d)) if d.is_ascii_digit() => {
                custom_values.insert(d, value.to_string());
            }
            _ => {
                if let Some((name, numeric)) = item_field(prefix) {
                    let value = match numeric {
                        true => number(value).unwrap_or_else(|| Value::String(value.into())),
                        false => Value::String(value.into()),
                    };
                    item.insert(name.into(), value);
                }
            }
        }
    }
    for (d, key) in custom_keys {
        if let Some(value) = custom_values.remove(&d) {
            item.entry(key).or_insert(Value::String(value));
        }
    }
    item
}

fn to_event(params: &[(String, String)], req: &IncomingRequest) -> Value {
    let mut ev = Map::new();
    let mut user_props = Map::new();
    let mut user = Map::new();
    let mut envelope = Map::new();
    let mut items = BTreeMap::new();
    let str_v = |v: &str| Value::String(v.to_string());

    for (k, v) in params {
        let key = k.as_str();
        match key {
            "en" => ev.insert("event_name".into(), str_v(v)),
            "cid" => ev.insert("client_id".into(), str_v(v)),
            "uid" => ev.insert("user_id".into(), str_v(v)),
            "dl" => ev.insert("page_location".into(), str_v(v)),
            "dr" => ev.insert("page_referrer".into(), str_v(v)),
            "dt" => ev.insert("page_title".into(), str_v(v)),
            "ul" => ev.insert("language".into(), str_v(v)),
            "sr" => ev.insert("screen_resolution".into(), str_v(v)),
            "cu" => ev.insert("currency".into(), str_v(v)),
            // Google sGTM keeps the session id a string and the count a number.
            "sid" => ev.insert("ga_session_id".into(), str_v(v)),
            "sct" => ev.insert(
                "ga_session_number".into(),
                number(v).unwrap_or_else(|| str_v(v)),
            ),
            "_et" => ev.insert(
                "engagement_time_msec".into(),
                number(v).unwrap_or_else(|| str_v(v)),
            ),
            "tid" => ev.insert("x-ga-measurement_id".into(), str_v(v)),
            "v" => ev.insert("x-ga-protocol_version".into(), str_v(v)),
            "_p" => ev.insert("x-ga-page_id".into(), str_v(v)),
            "gcs" => ev.insert("x-ga-gcs".into(), str_v(v)),
            "gcd" => ev.insert("x-ga-gcd".into(), str_v(v)),
            "seg" => ev.insert("x-ga-mp2-seg".into(), str_v(v)),
            "dma" => ev.insert("x-ga-dma".into(), str_v(v)),
            "dma_cps" => ev.insert("x-ga-dma_cps".into(), str_v(v)),
            "npa" => ev.insert("x-ga-npa".into(), str_v(v)),
            "em" => {
                user_envelope(&mut envelope, v);
                None
            }
            _ => {
                if let Some(path) = key.strip_prefix("ep.user_data.") {
                    user_param(&mut user, path, str_v(v));
                } else if let Some(path) = key.strip_prefix("epn.user_data.") {
                    user_param(&mut user, path, number(v).unwrap_or_else(|| str_v(v)));
                } else if let Some(name) = key.strip_prefix("ep.").filter(|n| !reserved(n)) {
                    ev.insert(name.into(), str_v(v));
                } else if let Some(name) = key.strip_prefix("epn.").filter(|n| !reserved(n)) {
                    ev.insert(name.into(), number(v).unwrap_or_else(|| str_v(v)));
                } else if let Some(name) = key.strip_prefix("up.") {
                    user_props.insert(name.into(), str_v(v));
                } else if let Some(name) = key.strip_prefix("upn.") {
                    user_props.insert(name.into(), number(v).unwrap_or_else(|| str_v(v)));
                } else if let Some(n) = key.strip_prefix("pr").and_then(|n| n.parse::<u32>().ok()) {
                    items.insert(n, Value::Object(to_item(v)));
                }
                None
            }
        };
    }

    if !user_props.is_empty() {
        ev.insert("x-ga-mp2-user_properties".into(), Value::Object(user_props));
    }
    // The tag's own user data wins over the envelope.
    merge_into(&mut envelope, user);
    if !envelope.is_empty() {
        ev.insert("user_data".into(), Value::Object(envelope));
    }
    if !items.is_empty() {
        ev.insert("items".into(), Value::Array(items.into_values().collect()));
    }
    // Always the address the request came from: a _uip sent with the hit is
    // ignored, so a client can't claim another visitor's IP or country.
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
    fn detects_mpv2() {
        assert!(is_mpv2(&req("/g/collect", "v=2&tid=G-1", "")));
        assert!(!is_mpv2(&req("/g/collect", "v=1&tid=G-1", "")));
        assert!(!is_mpv2(&req("/other", "v=2", "")));
    }

    #[test]
    fn maps_a_page_view() {
        let r = req(
            "/g/collect",
            "v=2&tid=G-TEST000001&cid=111.222&en=page_view&dl=https%3A%2F%2Fexample.com%2F&sid=1700000000&sct=3&ep.section=home&epn.value=9.5&up.plan=free&gcs=G111",
            "",
        );
        let ev = &extract_events(&r)[0];
        assert_eq!(ev["event_name"], "page_view");
        assert_eq!(ev["client_id"], "111.222");
        assert_eq!(ev["page_location"], "https://example.com/");
        assert_eq!(ev["ga_session_id"], "1700000000");
        assert_eq!(ev["ga_session_number"], 3);
        assert_eq!(ev["section"], "home");
        assert_eq!(ev["value"], 9.5);
        assert_eq!(ev["x-ga-mp2-user_properties"]["plan"], "free");
        assert_eq!(ev["x-ga-gcs"], "G111");
        assert_eq!(ev["ip_override"], "203.0.113.7");
        assert_eq!(ev["user_agent"], "Test UA");
        assert_eq!(ev["x-ga-measurement_id"], "G-TEST000001");
    }

    #[test]
    fn splits_a_batched_post_and_lines_override_the_query() {
        let r = req(
            "/g/collect",
            "v=2&tid=G-1&cid=1.2&en=page_view",
            "en=scroll&epn.percent_scrolled=90\r\nen=user_engagement&_et=1200",
        );
        let evs = extract_events(&r);
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0]["event_name"], "scroll");
        assert_eq!(evs[0]["percent_scrolled"], 90);
        assert_eq!(evs[0]["client_id"], "1.2");
        assert_eq!(evs[1]["event_name"], "user_engagement");
        assert_eq!(evs[1]["engagement_time_msec"], 1200);
        let raw = evs[1][RAW_HIT_KEY].as_str().unwrap();
        assert!(raw.contains("en=user_engagement") && !raw.contains("en=page_view"));
    }

    #[test]
    fn user_data_takes_sgtms_shape() {
        let hex = "b4c9a289323b21a01c3e940f150eb9b8c542587f1abfd8f0e1cc1ffc5e475514";
        // The same digest as url-safe base64, as the Google tag may send it.
        let b64 = "tMmiiTI7IaAcPpQPFQ65uMVCWH8av9jw4cwf_F5HVRQ";
        let r = req(
            "/g/collect",
            &format!(
                "v=2&en=purchase&em=tv.1~em.{b64}~ph.{}~ct.berlin~fn.&ep.user_data.phone=%2B49%20170%201234567&ep.user_data.address.first_name=Jane&ep.user_data.address.sha256_last_name=nothex&ep.user_data._tag_mode=MANUAL",
                hex.to_uppercase()
            ),
            "",
        );
        let ev = &extract_events(&r)[0];
        let u = &ev["user_data"];
        assert_eq!(u["sha256_email_address"], hex);
        assert_eq!(u["sha256_phone_number"], hex);
        assert_eq!(u["phone_number"], "+49 170 1234567");
        assert_eq!(u["address"]["first_name"], "Jane");
        assert_eq!(u["address"]["city"], "berlin");
        assert!(u["address"].get("sha256_last_name").is_none());
        assert!(u["address"].get("sha256_first_name").is_none());
        assert_eq!(u["_tag_mode"], "MANUAL");
        assert!(ev.get("user_data.phone").is_none());
        let plain = &extract_events(&req(
            "/g/collect",
            "v=2&en=x&ep.user_data.email_address=a%40b.c",
            "",
        ))[0];
        assert_eq!(plain["user_data"]["email_address"], "a@b.c");
        assert!(
            extract_events(&req("/g/collect", "v=2&en=x&em=tv.1~em", ""))[0]
                .get("user_data")
                .is_none()
        );
    }

    #[test]
    fn parameters_cant_set_the_servers_keys() {
        let ev = &extract_events(&req(
            "/g/collect",
            "v=2&en=x&gcd=13p3p3p2p5l1&ep.x-sst-source=shopify&epn.x-sst-bot-score=0&ep.x-ga-gcd=13r3r3r2r5l1&ep.ok=1",
            "",
        ))[0];
        assert!(ev.get("x-sst-source").is_none());
        assert!(ev.get("x-sst-bot-score").is_none());
        assert_eq!(
            ev["x-ga-gcd"], "13p3p3p2p5l1",
            "consent comes from gcd alone"
        );
        assert_eq!(ev["ok"], "1");
    }

    #[test]
    fn maps_ecommerce_items_in_order() {
        let r = req(
            "/g/collect",
            "v=2&tid=G-1&cid=1.2&en=purchase&cu=TRY\
             &pr2=idSKU2~nmMug~pr50~qt1\
             &pr1=idSKU1~nmT%20Shirt~brAcme~caApparel~c2Men~vaRed~pr19.90~qt2~cpSUMMER~ds5~lp3~k0size~v0M~zzjunk",
            "",
        );
        let ev = &extract_events(&r)[0];
        let items = ev["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["item_id"], "SKU1");
        assert_eq!(items[0]["item_name"], "T Shirt");
        assert_eq!(items[0]["item_brand"], "Acme");
        assert_eq!(items[0]["item_category"], "Apparel");
        assert_eq!(items[0]["item_category2"], "Men");
        assert_eq!(items[0]["item_variant"], "Red");
        assert_eq!(items[0]["price"], 19.9);
        assert_eq!(items[0]["quantity"], 2);
        assert_eq!(items[0]["coupon"], "SUMMER");
        assert_eq!(items[0]["discount"], 5);
        assert_eq!(items[0]["index"], 3);
        assert_eq!(items[0]["size"], "M");
        assert!(items[0].get("zzjunk").is_none() && items[0].get("zz").is_none());
        assert_eq!(items[1]["item_id"], "SKU2");
        assert_eq!(items[1]["price"], 50);
    }

    #[test]
    fn events_without_items_have_no_items_key() {
        let ev = &extract_events(&req("/g/collect", "v=2&en=page_view", ""))[0];
        assert!(ev.get("items").is_none());
    }

    #[test]
    fn uip_parameter_is_ignored() {
        let ev = &extract_events(&req("/g/collect", "v=2&en=x&_uip=198.51.100.1", ""))[0];
        assert_eq!(ev["ip_override"], "203.0.113.7");
    }
}
