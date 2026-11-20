//! What changed in the setup and when: every container save from the
//! console, with the file as it was before so it can be undone, and every
//! Integrations save, named only. Alerts look here for changes made just
//! before a problem started.
//!
//! Entries are `history/<id>.json` in the shared state, newest kept, so every
//! instance sees the same list.
//!
//! A kept file holds the container's settings, tokens and keys included, so
//! that an undo brings back a token changed by mistake. That is worth a
//! month: after it, the secrets are taken out of the kept file, so a token
//! that was replaced or removed doesn't stay in the state for as long as
//! 200 changes take. Undoing such an entry keeps the secrets live has now.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::container::Container;
use crate::state::{State, now_ms};

/// Most entries kept.
const KEEP: usize = 200;
/// How long a kept file holds its secrets.
const SECRETS_FOR_MS: u64 = 30 * 86_400_000;
/// Remembers how far the kept files have been gone through, so each is
/// read once and not on every check.
const SWEPT: &str = "history-secrets.json";
/// What a secret reads as once it is no longer kept, as in the console.
const REDACTED: &str = crate::container_file::REDACTED;

pub struct History {
    state: Arc<State>,
}

/// The file before a container save, so the save can be undone.
pub struct Before<'a> {
    pub revision: &'a str,
    pub text: &'a str,
}

/// An entry as lists show it: without the file it keeps, and whether it has
/// one to go back to.
fn listed(mut e: Value) -> Value {
    let undoable = e["before"].is_string();
    if let Some(m) = e.as_object_mut() {
        m.remove("before");
        m.insert("undoable".into(), json!(undoable));
    }
    e
}

impl History {
    pub fn new(state: Arc<State>) -> Self {
        History { state }
    }

    /// Keeps one entry. `edits` describe the change, as [`describe`] gives
    /// them; `before` is set for a container save.
    pub fn record(
        &self,
        source: &str,
        edits: Vec<Value>,
        before: Option<Before<'_>>,
        after_revision: Option<&str>,
    ) {
        self.write_entry(source, edits, before, after_revision, json!({}));
    }

    /// Keeps a published draft as a named version, with the file as it was
    /// before so it can be rolled back.
    pub fn record_version(
        &self,
        name: &str,
        note: &str,
        edits: Vec<Value>,
        before: Before<'_>,
        after_revision: &str,
    ) {
        let extra = json!({ "name": name, "note": note });
        self.write_entry("publish", edits, Some(before), Some(after_revision), extra);
    }

    fn write_entry(
        &self,
        source: &str,
        edits: Vec<Value>,
        before: Option<Before<'_>>,
        after_revision: Option<&str>,
        extra: Value,
    ) {
        let at = now_ms();
        // Two saves in one millisecond still get their own entries.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 10_000;
        let id = format!("{at:013}-{}-{seq:04}", self.state.instance);
        let entry = json!({
            "id": id,
            "at": at,
            "source": source,
            "edits": edits,
            "before_revision": before.as_ref().map(|b| b.revision),
            "after_revision": after_revision,
            "before": before.as_ref().map(|b| b.text),
        });
        let mut entry = entry;
        if let (Some(m), Value::Object(x)) = (entry.as_object_mut(), extra) {
            m.extend(x);
        }
        if let Err(e) = self.state.write(&format!("history/{id}.json"), &entry) {
            tracing::warn!("history: not kept: {e}");
        }
        let names = self.state.list("history");
        for old in names.iter().take(names.len().saturating_sub(KEEP)) {
            self.state.remove(&format!("history/{old}"));
        }
        self.expire_secrets();
    }

    /// Takes the secrets out of kept files older than a month. Run with
    /// every save and every alert check, so it happens without one too.
    pub fn expire_secrets(&self) {
        self.expire_secrets_at(now_ms());
    }

    pub(crate) fn expire_secrets_at(&self, now: u64) {
        let cutoff = now.saturating_sub(SECRETS_FOR_MS);
        let done = self
            .state
            .read(SWEPT)
            .and_then(|v| v["until"].as_u64())
            .unwrap_or(0);
        // An entry's name starts with the time it was made.
        let made = |name: &str| name.split('-').next().and_then(|at| at.parse::<u64>().ok());
        let due: Vec<(u64, String)> = self
            .state
            .list("history")
            .into_iter()
            .filter_map(|n| Some((made(&n)?, n)))
            .filter(|(at, _)| *at > done && *at <= cutoff)
            .collect();
        let Some(last) = due.iter().map(|(at, _)| *at).max() else {
            return;
        };
        for (_, name) in &due {
            let rel = format!("history/{name}");
            let Some(mut entry) = self.state.read(&rel) else {
                continue;
            };
            let Some(text) = entry["before"].as_str().map(str::to_string) else {
                continue;
            };
            match without_secrets(&text) {
                // Nothing secret in it: kept as it is.
                Some(None) => continue,
                Some(Some(clean)) => entry["before"] = json!(clean),
                // Not a file that can be gone through: not kept at all.
                None => entry["before"] = Value::Null,
            }
            entry["secrets_expired"] = json!(true);
            if let Err(e) = self.state.write(&rel, &entry) {
                tracing::warn!("history: secrets not taken out of {name}: {e}");
                return;
            }
        }
        if let Err(e) = self.state.write(SWEPT, &json!({ "until": last })) {
            tracing::warn!("history: {e}");
        }
    }

    /// Entries newest first, without the files they keep.
    pub fn list(&self, limit: usize) -> Vec<Value> {
        self.state
            .list("history")
            .iter()
            .rev()
            .take(limit)
            .filter_map(|n| self.state.read(&format!("history/{n}")))
            .map(listed)
            .collect()
    }

    /// Entries made at or after `from` (milliseconds), oldest first. An
    /// entry's name starts with the time it was made, so older ones are
    /// passed over without being read: each holds a whole container file.
    pub fn since(&self, from: u64) -> Vec<Value> {
        self.state
            .list("history")
            .iter()
            .filter(|n| {
                n.split('-')
                    .next()
                    .and_then(|at| at.parse::<u64>().ok())
                    .is_none_or(|at| at >= from)
            })
            .filter_map(|n| self.state.read(&format!("history/{n}")))
            .filter(|e| e["at"].as_u64().is_some_and(|at| at >= from))
            .map(listed)
            .collect()
    }

    /// The file as it was before entry `id`.
    pub fn before(&self, id: &str) -> Option<String> {
        if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return None;
        }
        self.state
            .read(&format!("history/{id}.json"))?
            .get("before")?
            .as_str()
            .map(str::to_string)
    }
}

/// The data of every tag and client in a container file, by list.
fn settings(file: &mut Value) -> impl Iterator<Item = (&'static str, &mut Value)> {
    let lists = file.as_object_mut().into_iter().flat_map(|m| m.iter_mut());
    lists
        .filter_map(|(k, v)| match k.as_str() {
            "tags" => Some(("tags", v)),
            "clients" => Some(("clients", v)),
            _ => None,
        })
        .flat_map(|(list, v)| {
            v.as_array_mut()
                .into_iter()
                .flatten()
                .map(move |entry| (list, entry))
        })
}

/// A container file's text with its secret settings taken out: `Some(None)`
/// when it holds none, None when the text isn't a container file.
fn without_secrets(text: &str) -> Option<Option<String>> {
    let mut file: Value = serde_json::from_str(text).ok()?;
    let mut changed = false;
    for (_, entry) in settings(&mut file) {
        if let Some(data) = entry.get_mut("data") {
            let clean = crate::admin::redact(data);
            changed |= clean != *data;
            *data = clean;
        }
    }
    if !changed {
        return Some(None);
    }
    serde_json::to_string_pretty(&file).ok().map(Some)
}

/// A kept file ready to go live again. Where its secrets are no longer
/// kept, each takes the value the same setting of the same tag or client
/// has in `live`, the file as it is now; one that has none there is left
/// empty, to be entered again. A kept file that still has its secrets comes
/// back as it is.
pub fn with_current_secrets(kept: &str, live: &str) -> String {
    if !kept.contains(REDACTED) {
        return kept.to_string();
    }
    let (Ok(mut file), Ok(mut live)) = (
        serde_json::from_str::<Value>(kept),
        serde_json::from_str::<Value>(live),
    ) else {
        return kept.to_string();
    };
    fn fill(v: &mut Value, now: Option<&Value>) {
        match v {
            Value::String(s) if s == REDACTED => {
                *v = match now {
                    Some(Value::String(n)) if n != REDACTED => json!(n),
                    _ => json!(""),
                }
            }
            Value::Object(m) => {
                for (k, x) in m.iter_mut() {
                    fill(x, now.and_then(|n| n.get(k)));
                }
            }
            Value::Array(a) => {
                for (i, x) in a.iter_mut().enumerate() {
                    fill(x, now.and_then(|n| n.get(i)));
                }
            }
            _ => {}
        }
    }
    let current: Vec<(&str, Value)> = settings(&mut live)
        .map(|(list, entry)| (list, entry.clone()))
        .collect();
    for (list, entry) in settings(&mut file) {
        let name = entry["name"].clone();
        let now = current
            .iter()
            .find(|(l, e)| *l == list && e["name"] == name)
            .map(|(_, e)| &e["data"]);
        if let Some(data) = entry.get_mut("data") {
            fill(data, now);
        }
    }
    serde_json::to_string_pretty(&file).unwrap_or_else(|_| kept.to_string())
}

/// What changed between two containers, as a list the console and alerts
/// put into words: tags added, deleted or changed (and which of their
/// fields), the tag order, and the same for transformations. Setting values
/// are never included, only their names, since some are secrets.
pub fn describe(old: &Container, new: &Container) -> Vec<Value> {
    let mut out = Vec::new();
    let old_tags: BTreeMap<&str, &crate::container::Tag> =
        old.tags.iter().map(|t| (t.name.as_str(), t)).collect();
    let new_tags: BTreeMap<&str, &crate::container::Tag> =
        new.tags.iter().map(|t| (t.name.as_str(), t)).collect();
    for (name, t) in &new_tags {
        let Some(o) = old_tags.get(name) else {
            out.push(json!({ "kind": "tag_added", "name": name }));
            continue;
        };
        let mut fields = Vec::new();
        if o.template != t.template {
            fields.push("template".to_string());
        }
        if o.enabled != t.enabled {
            fields.push(if t.enabled { "turned on" } else { "turned off" }.to_string());
        }
        if serde_json::to_value(&o.fire_when).ok() != serde_json::to_value(&t.fire_when).ok() {
            fields.push("fire_when".to_string());
        }
        let keys: std::collections::BTreeSet<&String> = o
            .data
            .as_object()
            .into_iter()
            .chain(t.data.as_object())
            .flat_map(|m| m.keys())
            .collect();
        for k in keys {
            if o.data.get(k) != t.data.get(k) {
                fields.push(format!("data.{k}"));
            }
        }
        if !fields.is_empty() {
            out.push(json!({ "kind": "tag_changed", "name": name, "fields": fields }));
        }
    }
    for name in old_tags.keys().filter(|n| !new_tags.contains_key(*n)) {
        out.push(json!({ "kind": "tag_deleted", "name": name }));
    }
    let order = |c: &Container, other: &BTreeMap<&str, &crate::container::Tag>| -> Vec<String> {
        c.tags
            .iter()
            .filter(|t| other.contains_key(t.name.as_str()))
            .map(|t| t.name.clone())
            .collect()
    };
    if order(old, &new_tags) != order(new, &old_tags) {
        out.push(json!({ "kind": "tag_order" }));
    }

    let clients = |c: &Container| -> BTreeMap<String, Value> {
        c.clients
            .iter()
            .map(|e| (e.name.clone(), e.data.clone()))
            .collect()
    };
    let new_clients = clients(new);
    for (name, old_data) in clients(old) {
        let Some(new_data) = new_clients.get(&name) else {
            continue;
        };
        let keys: std::collections::BTreeSet<&String> = old_data
            .as_object()
            .into_iter()
            .chain(new_data.as_object())
            .flat_map(|m| m.keys())
            .collect();
        let fields: Vec<String> = keys
            .into_iter()
            .filter(|k| old_data.get(*k) != new_data.get(*k))
            .map(|k| format!("data.{k}"))
            .collect();
        if !fields.is_empty() {
            out.push(json!({ "kind": "client_changed", "name": name, "fields": fields }));
        }
    }

    let rules = |c: &Container| -> BTreeMap<String, Value> {
        c.transformations
            .iter()
            .map(|r| (r.name.clone(), serde_json::to_value(r).unwrap_or_default()))
            .collect()
    };
    let (old_rules, new_rules) = (rules(old), rules(new));
    for (name, r) in &new_rules {
        match old_rules.get(name) {
            None => out.push(json!({ "kind": "transformation_added", "name": name })),
            Some(o) if o != r => {
                out.push(json!({ "kind": "transformation_changed", "name": name }))
            }
            _ => {}
        }
    }
    for name in old_rules.keys().filter(|n| !new_rules.contains_key(*n)) {
        out.push(json!({ "kind": "transformation_deleted", "name": name }));
    }
    let names = |c: &Container, other: &BTreeMap<String, Value>| -> Vec<String> {
        c.transformations
            .iter()
            .filter(|r| other.contains_key(&r.name))
            .map(|r| r.name.clone())
            .collect()
    };
    if names(old, &new_rules) != names(new, &old_rules) {
        out.push(json!({ "kind": "transformation_order" }));
    }
    out
}

/// One edit in plain English, for Slack and the AI.
pub fn sentence(e: &Value) -> String {
    let name = e["name"].as_str().unwrap_or_default();
    match e["kind"].as_str().unwrap_or_default() {
        "tag_added" => format!("added tag \"{name}\""),
        "tag_deleted" => format!("deleted tag \"{name}\""),
        "tag_changed" | "client_changed" => {
            let fields: Vec<&str> = e["fields"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let what = if e["kind"] == "tag_changed" {
                "tag"
            } else {
                "client"
            };
            format!("changed {what} \"{name}\" ({})", fields.join(", "))
        }
        "tag_order" => "changed the tag order".into(),
        "transformation_added" => format!("added transformation \"{name}\""),
        "transformation_deleted" => format!("deleted transformation \"{name}\""),
        "transformation_changed" => format!("changed transformation \"{name}\""),
        "transformation_order" => "changed the transformation order".into(),
        "integration_saved" => format!("saved {name} settings in Integrations"),
        "undo" => "undid an earlier change".into(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container(v: Value) -> Container {
        let dir = std::env::temp_dir();
        let mut c = Container::from_text(&v.to_string(), &dir.join("c.json")).unwrap();
        c.compile().unwrap();
        c
    }

    #[test]
    fn describes_what_changed_without_values() {
        let old = container(json!({ "clients": [], "tags": [
            { "name": "GA4", "template": "ga4_tag" },
            { "name": "Meta", "template": "meta_capi", "data": { "pixel_id": "1", "access_token": "a" } },
            { "name": "Old", "template": "ga4_tag" },
        ]}));
        let new = container(json!({ "clients": [], "tags": [
            { "name": "Meta", "template": "meta_capi", "enabled": false, "data": { "pixel_id": "1", "access_token": "b" } },
            { "name": "GA4", "template": "ga4_tag", "fire_when": [{ "key": "event_name", "op": "equals", "value": "x" }] },
            { "name": "New", "template": "ga4_tag" },
        ], "transformations": [{ "name": "T", "steps": [{ "op": "remove", "keys": ["x"] }] }] }));
        let d = describe(&old, &new);
        let s: Vec<String> = d.iter().map(sentence).collect();
        assert_eq!(
            s,
            vec![
                "changed tag \"GA4\" (fire_when)",
                "changed tag \"Meta\" (turned off, data.access_token)",
                "added tag \"New\"",
                "deleted tag \"Old\"",
                "changed the tag order",
                "added transformation \"T\"",
            ]
        );
        assert!(!serde_json::to_string(&d).unwrap().contains("\"b\""));
    }

    #[test]
    fn keeps_lists_and_gives_back_the_file_before() {
        let h = History::new(Arc::new(State::memory()));
        h.record(
            "console",
            vec![json!({ "kind": "tag_added", "name": "X" })],
            Some(Before {
                revision: "r1",
                text: "{old}",
            }),
            Some("r2"),
        );
        h.record(
            "integrations",
            vec![json!({ "kind": "integration_saved", "name": "Meta" })],
            None,
            None,
        );
        let l = h.list(10);
        assert_eq!(l.len(), 2);
        assert_eq!(l[0]["source"], "integrations");
        assert_eq!(l[0]["undoable"], false);
        assert_eq!(l[1]["undoable"], true);
        assert!(l[1].get("before").is_none());
        assert_eq!(
            h.before(l[1]["id"].as_str().unwrap()).as_deref(),
            Some("{old}")
        );
        assert!(h.before("../x").is_none());
        assert_eq!(h.since(0).len(), 2);
    }

    const DAY: u64 = 86_400_000;

    /// A container file with a token in a tag, one nested in it, and a
    /// client's signing secret.
    fn file_with(meta_token: &str, enrich_header: &str, webhook: &str) -> String {
        serde_json::to_string_pretty(&json!({
            "clients": [
                { "name": "GA4", "template": "ga4_client" },
                { "name": "Shopify", "template": "shopify_client", "data": { "webhook_secret": webhook, "item_id": "sku" } },
            ],
            "tags": [
                { "name": "GA4", "template": "ga4_tag" },
                { "name": "Meta", "template": "meta_capi", "data": {
                    "pixel_id": "123", "access_token": meta_token,
                    "enrich": { "url": "https://shop.example/api", "headerName": "x-api-key", "headerValue": enrich_header } } },
            ],
        }))
        .unwrap()
    }

    #[test]
    fn a_kept_file_holds_its_secrets_for_a_month() {
        let state = Arc::new(State::memory());
        let h = History::new(state.clone());
        let keep = |text: &str| {
            h.record(
                "console",
                vec![],
                Some(Before {
                    revision: "r1",
                    text,
                }),
                Some("r2"),
            );
        };
        let old = file_with("EAAB-old-token", "old-header-secret", "old-webhook-secret");
        keep(&old);
        keep(
            r#"{ "tags": [ { "name": "GA4", "template": "ga4_tag", "data": { "endpoint": "https://x.example/" } } ] }"#,
        );
        keep("not a container file, with a token: EAAB-loose");
        h.record("integrations", vec![], None, None);
        let ids: Vec<String> = h
            .list(10)
            .iter()
            .rev()
            .map(|e| e["id"].as_str().unwrap().to_string())
            .collect();
        let now = now_ms();

        // Within the month an undo gets the file as it was, token and all.
        h.expire_secrets_at(now + 29 * DAY);
        assert_eq!(h.before(&ids[0]).as_deref(), Some(old.as_str()));
        assert!(
            h.list(10)
                .iter()
                .all(|e| e.get("secrets_expired").is_none())
        );

        // After it, the secrets are gone from the state and the rest stays.
        h.expire_secrets_at(now + 31 * DAY);
        let kept = h.before(&ids[0]).unwrap();
        for secret in ["EAAB-old-token", "old-header-secret", "old-webhook-secret"] {
            assert!(!kept.contains(secret), "{secret} in {kept}");
        }
        let v: Value = serde_json::from_str(&kept).unwrap();
        assert_eq!(v["tags"][1]["data"]["access_token"], "[redacted]");
        assert_eq!(v["tags"][1]["data"]["enrich"]["headerValue"], "[redacted]");
        assert_eq!(v["clients"][1]["data"]["webhook_secret"], "[redacted]");
        assert_eq!(v["tags"][1]["data"]["pixel_id"], "123");
        assert_eq!(
            v["tags"][1]["data"]["enrich"]["url"],
            "https://shop.example/api"
        );
        assert_eq!(v["clients"][1]["data"]["item_id"], "sku");
        let all = state.read(&format!("history/{}.json", ids[0])).unwrap();
        assert!(!all.to_string().contains("EAAB-old-token"));

        let listed = h.list(10);
        let entry = |id: &String| listed.iter().find(|e| e["id"] == json!(id)).unwrap();
        assert_eq!(entry(&ids[0])["secrets_expired"], true);
        assert_eq!(entry(&ids[0])["undoable"], true);
        // A file without secrets is left exactly as it was kept.
        assert!(entry(&ids[1]).get("secrets_expired").is_none());
        assert!(h.before(&ids[1]).unwrap().contains("https://x.example/"));
        // One that can't be gone through isn't kept at all.
        assert_eq!(entry(&ids[2])["undoable"], false);
        assert!(h.before(&ids[2]).is_none());
        // An Integrations save never had a file.
        assert!(entry(&ids[3]).get("secrets_expired").is_none());

        // Each kept file is gone through once: what was done is remembered.
        let until = state.read(SWEPT).unwrap()["until"].as_u64().unwrap();
        assert!(until >= now.saturating_sub(1000) && until <= now_ms());
        h.expire_secrets_at(now + 60 * DAY);
        assert_eq!(state.read(SWEPT).unwrap()["until"], until);
    }

    #[test]
    fn undoing_an_old_change_keeps_the_secrets_live_has_now() {
        // What was kept, a month on.
        let kept = without_secrets(&file_with("EAAB-old-token", "old-header", "old-webhook"))
            .unwrap()
            .unwrap();
        // Live since then: the Meta token was rotated, and Shopify removed.
        let mut live: Value =
            serde_json::from_str(&file_with("EAAB-new-token", "new-header", "x")).unwrap();
        live["clients"].as_array_mut().unwrap().pop();
        let live = live.to_string();

        let restored = with_current_secrets(&kept, &live);
        assert!(!restored.contains("[redacted]"), "{restored}");
        let v: Value = serde_json::from_str(&restored).unwrap();
        // The tag still there gets what it has now, nested settings too.
        assert_eq!(v["tags"][1]["data"]["access_token"], "EAAB-new-token");
        assert_eq!(v["tags"][1]["data"]["enrich"]["headerValue"], "new-header");
        // The client that came back has nothing to take: entered again.
        assert_eq!(v["clients"][1]["name"], "Shopify");
        assert_eq!(v["clients"][1]["data"]["webhook_secret"], "");
        assert_eq!(v["clients"][1]["data"]["item_id"], "sku");

        // It loads and goes live like any undo.
        let dir = std::env::temp_dir().join(format!("history-undo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("container.json");
        std::fs::write(&path, &live).unwrap();
        let f = crate::container_file::ContainerFile::open(
            &path,
            Default::default(),
            Default::default(),
        )
        .unwrap();
        f.apply(
            &f.revision().unwrap(),
            crate::container_file::Change::Restore(restored),
            false,
        )
        .unwrap();
        let c = f.resolved().container;
        assert_eq!(c.tags[1].data["access_token"], "EAAB-new-token");
        assert_eq!(c.clients.len(), 2);
        std::fs::remove_dir_all(dir).ok();

        // A file kept within the month comes back byte for byte.
        let fresh = file_with("EAAB-old-token", "old-header", "old-webhook");
        assert_eq!(with_current_secrets(&fresh, &live), fresh);
    }
}
