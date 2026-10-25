//! What changed in the setup and when: every container save from the
//! console, with the file as it was before so it can be undone, and every
//! Integrations save, named only. Alerts look here for changes made just
//! before a problem started.
//!
//! Entries are `history/<id>.json` in the shared state, newest kept, so every
//! instance sees the same list.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::container::Container;
use crate::state::{State, now_ms};

/// Most entries kept.
const KEEP: usize = 200;

pub struct History {
    state: Arc<State>,
}

/// The file before a container save, so the save can be undone.
pub struct Before<'a> {
    pub revision: &'a str,
    pub text: &'a str,
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
    }

    /// Entries newest first, without the files they keep.
    pub fn list(&self, limit: usize) -> Vec<Value> {
        self.state
            .list("history")
            .iter()
            .rev()
            .take(limit)
            .filter_map(|n| self.state.read(&format!("history/{n}")))
            .map(|mut e| {
                let undoable = e["before"].is_string();
                if let Some(m) = e.as_object_mut() {
                    m.remove("before");
                    m.insert("undoable".into(), json!(undoable));
                }
                e
            })
            .collect()
    }

    /// Entries made at or after `from` (milliseconds), oldest first.
    pub fn since(&self, from: u64) -> Vec<Value> {
        let mut out: Vec<Value> = self
            .list(KEEP)
            .into_iter()
            .filter(|e| e["at"].as_u64().is_some_and(|at| at >= from))
            .collect();
        out.reverse();
        out
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
}
