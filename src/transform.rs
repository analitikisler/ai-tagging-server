//! Transformations: rules that change an event's data on the server before
//! tags see it, like the Transformations of Google's sGTM. A rule adds or
//! fixes values (set, copy, map through a table, format), or takes values
//! out (remove, keep only), for the events its conditions match and for the
//! tags it names, or every tag.
//!
//! Rules run in their order, each on the event as the rules before it left
//! it. A tag's firing conditions are checked against the event as its rules
//! changed it, so a rule that renames `Purchase` to `purchase` makes a tag
//! waiting for `purchase` fire. The client's event itself is never changed:
//! each tag gets its own copy.
//!
//! Keys are dotted for nested values (`user_data.address.city`), the same
//! way firing conditions name them.

use std::borrow::Cow;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::container::{Condition, compile_conditions};

/// Most rules a container holds, steps a rule holds and entries a table
/// holds, so a rule can't make every hit slow.
pub const MAX_RULES: usize = 100;
pub const MAX_STEPS: usize = 50;
pub const MAX_TABLE: usize = 10_000;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Transformation {
    pub name: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Every condition must match for the rule to run. None means every event.
    #[serde(default)]
    pub when: Vec<Condition>,
    /// The tags whose events it changes. None means every tag.
    #[serde(default)]
    pub tags: Vec<String>,
    pub steps: Vec<Step>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Step {
    /// Sets `key` to `value`; with `only_if_missing`, only when the event
    /// has no value there yet.
    Set {
        key: String,
        value: Value,
        #[serde(default, skip_serializing_if = "is_false")]
        only_if_missing: bool,
    },
    /// Copies the value at `from` to `to`, when there is one.
    Copy { from: String, to: String },
    /// Looks the value at `key` up in `table` (by its string form) and
    /// writes the result to `to`, or back to `key`. A value the table
    /// doesn't have gets `default`, or stays as it was.
    Map {
        key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<String>,
        table: BTreeMap<String, Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<Value>,
        #[serde(default, skip_serializing_if = "is_false")]
        ignore_case: bool,
    },
    /// Rewrites the value at `key` in a format.
    Format { key: String, format: Format },
    /// Takes the keys out of the event.
    Remove { keys: Vec<String> },
    /// Keeps only these keys, and `event_name`; everything else goes.
    Keep { keys: Vec<String> },
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    Lower,
    Upper,
    Trim,
    /// A number from a string such as `"49.90"` or `"49,90"`; a value that
    /// isn't one is left as it was.
    Number,
    String,
}

/// Checks a container's rules: names, limits, scopes and conditions.
/// `tags` are the container's tag names.
pub fn compile(rules: &mut [Transformation], tags: &[&str]) -> Result<(), String> {
    if rules.len() > MAX_RULES {
        return Err(format!("at most {MAX_RULES} transformations"));
    }
    let mut seen = std::collections::HashSet::new();
    for r in rules.iter_mut() {
        let name = r.name.trim();
        if name.is_empty() || name.chars().count() > 100 {
            return Err("a transformation needs a name of at most 100 characters".into());
        }
        if !seen.insert(name.to_string()) {
            return Err(format!("two transformations are called {name:?}"));
        }
        let err = |m: String| format!("transformation {name:?}: {m}");
        if r.steps.is_empty() || r.steps.len() > MAX_STEPS {
            return Err(err(format!("needs 1 to {MAX_STEPS} steps")));
        }
        for t in &r.tags {
            if !tags.contains(&t.as_str()) {
                return Err(err(format!("unknown tag {t:?}")));
            }
        }
        compile_conditions(&mut r.when).map_err(err)?;
        for s in &r.steps {
            let keys: Vec<&String> = match s {
                Step::Set { key, .. } | Step::Format { key, .. } => vec![key],
                Step::Copy { from, to } => vec![from, to],
                Step::Map { key, to, table, .. } => {
                    if table.len() > MAX_TABLE {
                        return Err(err(format!("a table holds at most {MAX_TABLE} entries")));
                    }
                    std::iter::once(key).chain(to).collect()
                }
                Step::Remove { keys } | Step::Keep { keys } => keys.iter().collect(),
            };
            if keys.iter().any(|k| k.split('.').any(str::is_empty)) {
                return Err(err(
                    "every key needs a name, dotted for nested values".into()
                ));
            }
        }
    }
    Ok(())
}

impl Transformation {
    fn applies(&self, tag: &str, event: &Value) -> bool {
        self.enabled
            && (self.tags.is_empty() || self.tags.iter().any(|t| t == tag))
            && self.when.iter().all(|c| c.matches(event))
    }
}

/// The event `tag` sees, and the names of the rules that ran for it. Without
/// a rule for it, the event is borrowed as it is.
pub fn view<'a>(
    rules: &[Transformation],
    tag: &str,
    event: &'a Value,
) -> (Cow<'a, Value>, Vec<String>) {
    let mut out = Cow::Borrowed(event);
    let mut ran = Vec::new();
    for r in rules {
        if !r.applies(tag, &out) {
            continue;
        }
        let e = out.to_mut();
        for s in &r.steps {
            step(s, e);
        }
        ran.push(r.name.clone());
    }
    (out, ran)
}

/// Most changed events a preview shows.
const PREVIEW_SAMPLES: usize = 5;

/// What `rule` would do to `events` (trace id and event): how many it
/// matches and changes, and a few changes key by key. The rule runs on each
/// event as the client built it, without the rules before it.
pub fn preview_on(rule: &Transformation, events: &[(u64, Value)]) -> Value {
    let (mut matched, mut changed) = (0, 0);
    let mut samples = Vec::new();
    for (id, event) in events {
        let Some(after) = preview(rule, event) else {
            continue;
        };
        matched += 1;
        let diff = diff(event, &after);
        if diff.is_empty() {
            continue;
        }
        changed += 1;
        if samples.len() < PREVIEW_SAMPLES {
            samples.push(json!({
                "trace": id,
                "event_name": event["event_name"],
                "changes": diff,
            }));
        }
    }
    json!({ "events": events.len(), "matched": matched, "changed": changed, "samples": samples })
}

/// Every key whose value differs, dotted, with the value before and after
/// (null when there was none, or is none now).
fn diff(before: &Value, after: &Value) -> Vec<Value> {
    let (a, b) = (flatten(before), flatten(after));
    let mut keys: Vec<&String> = a.keys().chain(b.keys()).collect();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .filter(|k| a.get(*k) != b.get(*k))
        .map(|k| json!({ "key": k, "before": a.get(k), "after": b.get(k) }))
        .collect()
}

fn flatten(v: &Value) -> BTreeMap<String, Value> {
    fn walk(v: &Value, at: &str, out: &mut BTreeMap<String, Value>) {
        match v {
            Value::Object(m) if !m.is_empty() => {
                for (k, x) in m {
                    let key = if at.is_empty() {
                        k.clone()
                    } else {
                        format!("{at}.{k}")
                    };
                    walk(x, &key, out);
                }
            }
            _ if !at.is_empty() => {
                out.insert(at.to_string(), v.clone());
            }
            _ => {}
        }
    }
    let mut out = BTreeMap::new();
    walk(v, "", &mut out);
    out
}

/// What one rule does to one event, without the rules before it; None when
/// it doesn't run for the event.
pub fn preview(rule: &Transformation, event: &Value) -> Option<Value> {
    if !rule.enabled || !rule.when.iter().all(|c| c.matches(event)) {
        return None;
    }
    let mut after = event.clone();
    for s in &rule.steps {
        step(s, &mut after);
    }
    Some(after)
}

fn get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    key.split('.').try_fold(v, |v, part| v.get(part))
}

/// Writes `value` at `key`, making the objects on the way. A value that is
/// in the way and isn't an object is replaced.
fn put(v: &mut Value, key: &str, value: Value) {
    let mut parts = key.split('.').peekable();
    let mut at = v;
    while let Some(part) = parts.next() {
        if !at.is_object() {
            *at = Value::Object(Map::new());
        }
        let m = at.as_object_mut().expect("object");
        if parts.peek().is_none() {
            m.insert(part.to_string(), value);
            return;
        }
        at = m.entry(part).or_insert_with(|| json!({}));
    }
}

fn take(v: &mut Value, key: &str) -> Option<Value> {
    let (parent, last) = match key.rsplit_once('.') {
        Some((p, l)) => (
            p.split('.').try_fold(&mut *v, |v, part| v.get_mut(part))?,
            l,
        ),
        None => (v, key),
    };
    parent.as_object_mut()?.remove(last)
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn step(s: &Step, e: &mut Value) {
    match s {
        Step::Set {
            key,
            value,
            only_if_missing,
        } => {
            if !*only_if_missing || get(e, key).is_none_or(Value::is_null) {
                put(e, key, value.clone());
            }
        }
        Step::Copy { from, to } => {
            if let Some(v) = get(e, from).cloned() {
                put(e, to, v);
            }
        }
        Step::Map {
            key,
            to,
            table,
            default,
            ignore_case,
        } => {
            let Some(v) = get(e, key) else {
                return;
            };
            let k = text(v);
            let found = table.get(&k).or_else(|| {
                ignore_case
                    .then(|| {
                        table
                            .iter()
                            .find(|(t, _)| t.eq_ignore_ascii_case(&k))
                            .map(|(_, v)| v)
                    })
                    .flatten()
            });
            if let Some(out) = found.or(default.as_ref()).cloned() {
                put(e, to.as_deref().unwrap_or(key), out);
            }
        }
        Step::Format { key, format } => {
            let Some(v) = get(e, key) else {
                return;
            };
            let out = match (format, v) {
                (Format::Lower, Value::String(s)) => json!(s.to_lowercase()),
                (Format::Upper, Value::String(s)) => json!(s.to_uppercase()),
                (Format::Trim, Value::String(s)) => json!(s.trim()),
                (Format::Number, Value::String(s)) => {
                    let s = s.trim();
                    let s = if s.contains(',') && !s.contains('.') {
                        s.replace(',', ".")
                    } else {
                        s.replace(',', "")
                    };
                    match s.parse::<f64>() {
                        Ok(n) if n.is_finite() => {
                            if n.fract() == 0.0 && n.abs() < 1e15 {
                                json!(n as i64)
                            } else {
                                json!(n)
                            }
                        }
                        _ => return,
                    }
                }
                (Format::String, v) if !v.is_string() && !v.is_object() && !v.is_array() => {
                    json!(text(v))
                }
                _ => return,
            };
            put(e, key, out);
        }
        Step::Remove { keys } => {
            for k in keys {
                take(e, k);
            }
        }
        Step::Keep { keys } => {
            let mut kept = json!({});
            for k in keys.iter().map(String::as_str).chain(["event_name"]) {
                if let Some(v) = get(e, k) {
                    put(&mut kept, k, v.clone());
                }
            }
            *e = kept;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(v: Value) -> Vec<Transformation> {
        let mut r: Vec<Transformation> = serde_json::from_value(v).unwrap();
        compile(&mut r, &["GA4", "Meta"]).unwrap();
        r
    }

    #[test]
    fn steps_change_a_copy_for_the_tags_in_scope() {
        let r = rules(json!([
            { "name": "Normalise", "steps": [
                { "op": "format", "key": "event_name", "format": "lower" },
                { "op": "format", "key": "value", "format": "number" },
                { "op": "set", "key": "currency", "value": "EUR", "only_if_missing": true },
                { "op": "copy", "from": "user_id", "to": "user_data.external_id" },
            ]},
            { "name": "Margin for Meta", "tags": ["Meta"], "when": [{ "key": "event_name", "op": "equals", "value": "purchase" }], "steps": [
                { "op": "map", "key": "items_category", "to": "margin", "table": { "shoes": 0.4 }, "default": 0.2, "ignore_case": true },
                { "op": "remove", "keys": ["user_id"] },
            ]},
        ]));
        let event = json!({ "event_name": "Purchase", "value": "49,90", "user_id": "u1", "items_category": "SHOES" });
        let (ga4, ran) = view(&r, "GA4", &event);
        assert_eq!(ran, vec!["Normalise"]);
        assert_eq!(
            *ga4,
            json!({ "event_name": "purchase", "value": 49.9, "currency": "EUR", "user_id": "u1", "items_category": "SHOES", "user_data": { "external_id": "u1" } })
        );
        let (meta, ran) = view(&r, "Meta", &event);
        assert_eq!(
            ran.len(),
            2,
            "the second rule sees the lower-cased event name"
        );
        assert_eq!(meta["margin"], 0.4);
        assert!(meta.get("user_id").is_none());
        assert_eq!(
            event["event_name"], "Purchase",
            "the client's event is never changed"
        );
    }

    #[test]
    fn no_rule_borrows_the_event() {
        let r = rules(
            json!([{ "name": "Meta only", "tags": ["Meta"], "steps": [{ "op": "remove", "keys": ["x"] }] }]),
        );
        let event = json!({ "event_name": "page_view" });
        assert!(matches!(view(&r, "GA4", &event).0, Cow::Borrowed(_)));
    }

    #[test]
    fn keep_only_listed_keys() {
        let r = rules(
            json!([{ "name": "Minimal", "steps": [{ "op": "keep", "keys": ["value", "user_data.sha256_email_address"] }] }]),
        );
        let event = json!({ "event_name": "purchase", "value": 5, "ip_override": "1.2.3.4", "user_data": { "sha256_email_address": "ab", "email_address": "x@y" } });
        assert_eq!(
            *view(&r, "GA4", &event).0,
            json!({ "event_name": "purchase", "value": 5, "user_data": { "sha256_email_address": "ab" } })
        );
    }

    #[test]
    fn number_format_leaves_what_isnt_a_number() {
        let r = rules(json!([{ "name": "N", "steps": [
            { "op": "format", "key": "a", "format": "number" },
            { "op": "format", "key": "b", "format": "number" },
            { "op": "format", "key": "c", "format": "number" },
            { "op": "format", "key": "d", "format": "string" },
        ] }]));
        let e = json!({ "a": "1,234.50", "b": "n/a", "c": "12", "d": 7 });
        assert_eq!(
            *view(&r, "GA4", &e).0,
            json!({ "a": 1234.5, "b": "n/a", "c": 12, "d": "7" })
        );
    }

    #[test]
    fn a_preview_counts_and_shows_changes() {
        let mut r: Vec<Transformation> = serde_json::from_value(json!([{ "name": "P",
            "when": [{ "key": "event_name", "op": "equals", "value": "Purchase" }],
            "steps": [{ "op": "format", "key": "event_name", "format": "lower" }, { "op": "set", "key": "x.y", "value": 1 }] }])).unwrap();
        compile(&mut r, &[]).unwrap();
        let events = vec![
            (1, json!({ "event_name": "Purchase" })),
            (2, json!({ "event_name": "page_view" })),
        ];
        let p = preview_on(&r[0], &events);
        assert_eq!(p["matched"], 1);
        assert_eq!(p["changed"], 1);
        assert_eq!(p["samples"][0]["trace"], 1);
        assert_eq!(
            p["samples"][0]["changes"],
            json!([
                { "key": "event_name", "before": "Purchase", "after": "purchase" },
                { "key": "x.y", "before": null, "after": 1 },
            ])
        );
    }

    #[test]
    fn bad_rules_are_refused() {
        let check = |v: Value| {
            let mut r: Vec<Transformation> = serde_json::from_value(v).unwrap();
            compile(&mut r, &["GA4"])
        };
        assert!(check(json!([{ "name": "x", "tags": ["Nope"], "steps": [{ "op": "remove", "keys": ["a"] }] }])).unwrap_err().contains("unknown tag"));
        assert!(check(json!([{ "name": "x", "steps": [] }])).is_err());
        assert!(
            check(json!([{ "name": "x", "steps": [{ "op": "remove", "keys": ["a..b"] }] }]))
                .is_err()
        );
        assert!(check(json!([{ "name": "x", "when": [{ "key": "a", "op": "matches_regex", "value": "(" }], "steps": [{ "op": "remove", "keys": ["a"] }] }])).is_err());
        assert!(
            check(json!([
                { "name": "x", "steps": [{ "op": "remove", "keys": ["a"] }] },
                { "name": "x", "steps": [{ "op": "remove", "keys": ["b"] }] },
            ]))
            .unwrap_err()
            .contains("two")
        );
        assert!(
            serde_json::from_value::<Vec<Transformation>>(
                json!([{ "name": "x", "steps": [{ "op": "explode" }] }])
            )
            .is_err()
        );
    }
}
