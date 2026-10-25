//! The container a server runs in single and integrated mode, and the edits
//! the admin console makes to it. With `CONTAINER_FILE` the file is the
//! source of truth: a save rewrites its `tags` or its `transformations` and
//! nothing else, and every
//! instance re-reads it when it changes. The built-in container can't be
//! edited.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::container::{Container, Tag};
use crate::sandbox::Storage;
use crate::tenants::{Resolved, TenantStats};

/// What the console sends back for a secret it was shown redacted.
pub const REDACTED: &str = "[redacted]";
/// Longest tag name accepted.
const MAX_NAME: usize = 100;

/// Why an edit was refused.
#[derive(Debug, PartialEq)]
pub enum EditError {
    /// The container isn't a file, so there is nothing to save to.
    ReadOnly(&'static str),
    /// The change doesn't validate; nothing was written.
    Invalid(String),
    /// No tag has that name.
    NotFound,
    /// The file changed since it was loaded; its current revision.
    Conflict(String),
    /// The file couldn't be read or written.
    Io(String),
}

struct State {
    modified: Option<SystemTime>,
    /// Hash of the file's bytes, None for the built-in container.
    revision: Option<String>,
    resolved: Resolved,
}

pub struct ContainerFile {
    /// None for the built-in container.
    path: Option<PathBuf>,
    storage: Storage,
    stats: Arc<TenantStats>,
    state: RwLock<State>,
    generation: AtomicU64,
    /// Serialises admin writes.
    write: Mutex<()>,
}

/// Short hex digest of a file's bytes, so a save can tell whether the file
/// changed since the console loaded it.
pub fn revision_of(bytes: &[u8]) -> String {
    Sha256::digest(bytes)[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn modified_at(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl ContainerFile {
    /// A container that isn't backed by a file (the built-in one).
    pub fn fixed(container: Container, storage: Storage, stats: Arc<TenantStats>) -> Arc<Self> {
        Arc::new(Self::new(None, container, storage, stats))
    }

    /// Loads the file, refusing one that doesn't validate.
    pub fn open(
        path: &Path,
        storage: Storage,
        stats: Arc<TenantStats>,
    ) -> Result<Arc<Self>, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let container = Container::from_text(&String::from_utf8_lossy(&bytes), path)?;
        let me = Self::new(Some(path.to_path_buf()), container, storage, stats);
        {
            let mut state = me.state.write().expect("container file");
            state.modified = modified_at(path);
            state.revision = Some(revision_of(&bytes));
        }
        Ok(Arc::new(me))
    }

    fn new(
        path: Option<PathBuf>,
        container: Container,
        storage: Storage,
        stats: Arc<TenantStats>,
    ) -> Self {
        ContainerFile {
            path,
            state: RwLock::new(State {
                modified: None,
                revision: None,
                resolved: Resolved {
                    id: "default".into(),
                    generation: 1,
                    container: Arc::new(container),
                    storage: storage.clone(),
                    stats: stats.clone(),
                },
            }),
            storage,
            stats,
            generation: AtomicU64::new(2),
            write: Mutex::default(),
        }
    }

    /// What a request should run against right now.
    pub fn resolved(&self) -> Resolved {
        self.state.read().expect("container file").resolved.clone()
    }

    /// Changes whenever the container does.
    pub fn generation(&self) -> u64 {
        self.state
            .read()
            .expect("container file")
            .resolved
            .generation
    }

    /// The file's current revision; None when there is no file.
    pub fn revision(&self) -> Option<String> {
        self.state.read().expect("container file").revision.clone()
    }

    pub fn is_file(&self) -> bool {
        self.path.is_some()
    }

    /// Re-reads the file when its modification time changed. A file that no
    /// longer validates is ignored with a warning, and the container already
    /// running keeps serving.
    pub fn reload_if_changed(&self) {
        let Some(path) = &self.path else { return };
        let modified = modified_at(path);
        if modified == self.state.read().expect("container file").modified {
            return;
        }
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                self.state.write().expect("container file").modified = modified;
                tracing::warn!("container file not reloaded: {}: {e}", path.display());
                return;
            }
        };
        let revision = revision_of(&bytes);
        if self.revision().as_deref() == Some(revision.as_str()) {
            self.state.write().expect("container file").modified = modified;
            return;
        }
        match Container::from_text(&String::from_utf8_lossy(&bytes), path) {
            Ok(c) => {
                self.install(c, modified, revision);
                tracing::info!("container reloaded from {}", path.display());
            }
            Err(e) => {
                // Remember the bad version so the warning isn't repeated.
                self.state.write().expect("container file").modified = modified;
                tracing::warn!("container file not reloaded: {e}");
            }
        }
    }

    fn install(&self, container: Container, modified: Option<SystemTime>, revision: String) {
        let mut state = self.state.write().expect("container file");
        state.modified = modified;
        state.revision = Some(revision);
        // Workers keep their compiled runtime until the generation changes.
        state.resolved = Resolved {
            id: "default".into(),
            generation: self.generation.fetch_add(1, Relaxed),
            container: Arc::new(container),
            storage: self.storage.clone(),
            stats: self.stats.clone(),
        };
    }

    /// Adds a tag at the end, so it fires after the others. These helpers
    /// are for tests; the console goes through [`ContainerFile::apply`].
    #[cfg(test)]
    pub fn add(&self, revision: &str, tag: Tag) -> Result<(), EditError> {
        self.apply(revision, Change::Add(tag), false).map(|_| ())
    }

    /// Replaces the tag called `name`; `tag.name` may rename it. Settings
    /// still `[redacted]` keep the value stored at the same place.
    #[cfg(test)]
    pub fn update(&self, revision: &str, name: &str, tag: Tag) -> Result<(), EditError> {
        self.apply(revision, Change::Update(name.to_string(), tag), false)
            .map(|_| ())
    }

    #[cfg(test)]
    pub fn delete(&self, revision: &str, name: &str) -> Result<(), EditError> {
        self.apply(revision, Change::Delete(name.to_string()), false)
            .map(|_| ())
    }

    /// Puts the tags in the given order, which is the order they fire in.
    /// `names` must list every tag once.
    #[cfg(test)]
    pub fn reorder(&self, revision: &str, names: &[String]) -> Result<(), EditError> {
        self.apply(revision, Change::Reorder(names.to_vec()), false)
            .map(|_| ())
    }

    /// Replaces the transformations with `rules`, in their order.
    #[cfg(test)]
    pub fn set_transformations(&self, revision: &str, rules: Vec<Value>) -> Result<(), EditError> {
        self.apply(revision, Change::Transformations(rules), false)
            .map(|_| ())
    }

    /// The file's text after `change`, worked out on `before` (the file, or
    /// a draft of it), and the container it loads as, checked exactly as a
    /// startup would. Nothing is written.
    pub fn compute(&self, before: &str, change: Change) -> Result<(String, Container), EditError> {
        let Some(path) = &self.path else {
            return Err(EditError::ReadOnly(
                "the built-in container can't be edited",
            ));
        };
        let before = before.to_string();
        let next = match change {
            Change::Restore(text) => text,
            Change::Setup {
                tags,
                transformations,
            } => {
                let next = edit_tags(&before, |list| {
                    let old = std::mem::take(list);
                    for tag in tags {
                        // A tag that keeps its name keeps its stored secrets.
                        let prev =
                            position(&old, &tag.name).and_then(|i| old[i].as_object().cloned());
                        let mut data = tag.data.clone();
                        let at = format!("{}.data", tag.name);
                        keep_redacted(&mut data, prev.as_ref().and_then(|p| p.get("data")), &at)
                            .map_err(EditError::Invalid)?;
                        list.push(tag_json(&tag, data, prev.unwrap_or_default()));
                    }
                    Ok(())
                })?;
                edit_list(&next, "transformations", |list| {
                    *list = transformations;
                    Ok(())
                })?
            }
            Change::Transformations(rules) => edit_list(&before, "transformations", |list| {
                *list = rules;
                Ok(())
            })?,
            Change::ClientFeatures(name, features) => edit_list(&before, "clients", |clients| {
                let at = position(clients, &name).ok_or(EditError::NotFound)?;
                let client = clients[at].as_object_mut().ok_or_else(|| {
                    EditError::Invalid(format!("client {name:?} isn't an object"))
                })?;
                let data = client
                    .entry("data")
                    .or_insert_with(|| Value::Object(Map::new()));
                let data = data
                    .as_object_mut()
                    .ok_or_else(|| EditError::Invalid(format!("{name}.data isn't an object")))?;
                for k in [
                    "keepCookies",
                    "clickIdAliases",
                    "visitorInfo",
                    "botHandling",
                ] {
                    data.remove(k);
                }
                data.extend(features.client_data());
                Ok(())
            })?,
            Change::Add(tag) => edit_tags(&before, |tags| {
                let mut data = tag.data.clone();
                keep_redacted(&mut data, None, "data").map_err(EditError::Invalid)?;
                tags.push(tag_json(&tag, data, Map::new()));
                Ok(())
            })?,
            Change::Update(name, tag) => edit_tags(&before, |tags| {
                let at = position(tags, &name).ok_or(EditError::NotFound)?;
                let old = tags[at].as_object().cloned().unwrap_or_default();
                let mut data = tag.data.clone();
                keep_redacted(&mut data, old.get("data"), "data").map_err(EditError::Invalid)?;
                tags[at] = tag_json(&tag, data, old);
                Ok(())
            })?,
            Change::Delete(name) => edit_tags(&before, |tags| {
                let at = position(tags, &name).ok_or(EditError::NotFound)?;
                tags.remove(at);
                Ok(())
            })?,
            Change::Reorder(names) => edit_tags(&before, |tags| {
                let mut rest = std::mem::take(tags);
                for n in &names {
                    let at = position(&rest, n).ok_or_else(|| {
                        EditError::Invalid(format!("no tag {n:?}, or it is listed twice"))
                    })?;
                    tags.push(rest.remove(at));
                }
                if !rest.is_empty() {
                    return Err(EditError::Invalid(
                        "the new order must list every tag".into(),
                    ));
                }
                Ok(())
            })?,
        };
        let container = Container::from_text(&next, path).map_err(EditError::Invalid)?;
        Ok((next, container))
    }

    /// The file's text and revision as it is on disk now.
    pub fn text(&self) -> Result<(String, String), EditError> {
        let Some(path) = &self.path else {
            return Err(EditError::ReadOnly(
                "the built-in container can't be edited",
            ));
        };
        let bytes =
            std::fs::read(path).map_err(|e| EditError::Io(format!("{}: {e}", path.display())))?;
        Ok((
            String::from_utf8_lossy(&bytes).into_owned(),
            revision_of(&bytes),
        ))
    }

    /// Checks `change` against the file at `revision` and compiles the
    /// result exactly as a startup would. With `dry`, nothing is written:
    /// the result is only returned, for a check before saving. Otherwise the
    /// file is saved and the new container runs.
    pub fn apply(&self, revision: &str, change: Change, dry: bool) -> Result<Applied, EditError> {
        let Some(path) = &self.path else {
            return Err(EditError::ReadOnly(
                "the built-in container can't be edited",
            ));
        };
        let _w = self.write.lock().expect("write");
        let bytes =
            std::fs::read(path).map_err(|e| EditError::Io(format!("{}: {e}", path.display())))?;
        let current = revision_of(&bytes);
        if current != revision {
            return Err(EditError::Conflict(current));
        }
        let before = String::from_utf8_lossy(&bytes).into_owned();
        let (next, container) = self.compute(&before, change)?;
        let applied = Applied {
            before_revision: current,
            after_revision: revision_of(next.as_bytes()),
            before,
            after: next,
            container,
        };
        if dry {
            return Ok(applied);
        }

        // A temporary file in the same directory, renamed over the old one, so
        // a crash never leaves half a file for the other instances to read.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &applied.after)
            .map_err(|e| EditError::Io(format!("{}: {e}", tmp.display())))?;
        std::fs::rename(&tmp, path)
            .map_err(|e| EditError::Io(format!("{}: {e}", path.display())))?;
        self.install(
            applied.container.clone(),
            modified_at(path),
            applied.after_revision.clone(),
        );
        Ok(applied)
    }
}

/// One edit to a container file.
pub enum Change {
    Add(Tag),
    /// The tag's current name, and the tag as it should be.
    Update(String, Tag),
    Delete(String),
    Reorder(Vec<String>),
    Transformations(Vec<Value>),
    /// The whole file as it should be, from the container history.
    Restore(String),
    /// Every tag and transformation, in order, as a whole new setup.
    Setup {
        tags: Vec<Tag>,
        transformations: Vec<Value>,
    },
    /// A client's optional features (see [`crate::features`]); the rest of
    /// its data stays as it is.
    ClientFeatures(String, crate::features::Features),
}

/// An edit worked out: the file before and after, and the container it runs.
pub struct Applied {
    pub before_revision: String,
    pub after_revision: String,
    pub before: String,
    pub after: String,
    pub container: Container,
}

fn edit_tags(
    text: &str,
    change: impl FnOnce(&mut Vec<Value>) -> Result<(), EditError>,
) -> Result<String, EditError> {
    edit_list(text, "tags", |tags| {
        change(tags)?;
        check_names(tags).map_err(EditError::Invalid)
    })
}

/// The file's text with `change` applied to its top-level `key` list. Every
/// other key is kept as written; a list the file doesn't have yet is added
/// at its end.
fn edit_list(
    text: &str,
    key: &str,
    change: impl FnOnce(&mut Vec<Value>) -> Result<(), EditError>,
) -> Result<String, EditError> {
    let mut text = text.to_string();
    if key != "tags" && list_span(&text, key).is_none() {
        let close = text
            .rfind('}')
            .ok_or_else(|| EditError::Invalid("the file isn't a JSON object".into()))?;
        let body_end = text[..close].trim_end().len();
        text.insert_str(body_end, &format!(",\n  \"{key}\": []"));
    }
    let (start, end) = list_span(&text, key)
        .ok_or_else(|| EditError::Invalid(format!("the file has no top-level \"{key}\" list")))?;
    let mut list: Vec<Value> = serde_json::from_str(&text[start..end])
        .map_err(|e| EditError::Invalid(format!("{key}: {e}")))?;
    change(&mut list)?;

    let line_start = text[..start].rfind('\n').map_or(0, |i| i + 1);
    let indent: String = text[line_start..]
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect();
    let pretty = serde_json::to_string_pretty(&list).map_err(|e| EditError::Io(e.to_string()))?;
    let pretty = pretty.replace('\n', &format!("\n{indent}"));
    Ok(format!("{}{pretty}{}", &text[..start], &text[end..]))
}

fn position(tags: &[Value], name: &str) -> Option<usize> {
    tags.iter().position(|t| t["name"].as_str() == Some(name))
}

/// Names are required, at most 100 characters and unique.
fn check_names(tags: &[Value]) -> Result<(), String> {
    let mut seen = HashSet::new();
    for t in tags {
        let name = t["name"].as_str().unwrap_or_default();
        if name.trim().is_empty() {
            return Err("every tag needs a name".into());
        }
        if name.chars().count() > MAX_NAME || name.chars().any(char::is_control) {
            return Err(format!(
                "tag names are at most {MAX_NAME} characters, without line breaks"
            ));
        }
        if !seen.insert(name) {
            return Err(format!("there is already a tag called {name:?}"));
        }
    }
    Ok(())
}

/// The tag as the file stores it. Keys of `old` the console doesn't know
/// about stay where they were; `enabled` is written only when it is off.
fn tag_json(tag: &Tag, data: Value, mut old: Map<String, Value>) -> Value {
    old.insert("name".into(), Value::String(tag.name.trim().to_string()));
    old.insert("template".into(), Value::String(tag.template.clone()));
    let empty = data.as_object().is_some_and(Map::is_empty);
    if empty && !old.contains_key("data") {
        old.shift_remove("data");
    } else {
        old.insert("data".into(), data);
    }
    if tag.fire_when.is_empty() && !old.contains_key("fire_when") {
        old.shift_remove("fire_when");
    } else {
        old.insert(
            "fire_when".into(),
            serde_json::to_value(&tag.fire_when).unwrap_or_default(),
        );
    }
    if tag.enabled {
        old.shift_remove("enabled");
    } else {
        old.insert("enabled".into(), Value::Bool(false));
    }
    Value::Object(old)
}

/// Puts back the stored value wherever the console sent `[redacted]`. There
/// has to be one at the same place, or the save is refused.
fn keep_redacted(new: &mut Value, old: Option<&Value>, at: &str) -> Result<(), String> {
    match new {
        Value::String(s) if s == REDACTED => match old {
            Some(v) if !v.is_null() && v.as_str() != Some(REDACTED) => {
                *new = v.clone();
                Ok(())
            }
            _ => Err(format!("{at}: no stored value to keep, enter it again")),
        },
        Value::Object(m) => {
            for (k, v) in m.iter_mut() {
                keep_redacted(v, old.and_then(|o| o.get(k)), &format!("{at}.{k}"))?;
            }
            Ok(())
        }
        Value::Array(a) => {
            for (i, v) in a.iter_mut().enumerate() {
                keep_redacted(v, old.and_then(|o| o.get(i)), &format!("{at}[{i}]"))?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// End of the JSON string starting at `start` (just past its closing quote).
fn string_end(b: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// Byte range of the top-level `"tags"` list in the file's text, so a save
/// replaces exactly that and leaves the rest as written.
#[cfg(test)]
fn tags_span(text: &str) -> Option<(usize, usize)> {
    list_span(text, "tags")
}

/// Byte range of the top-level list called `key` in the file's text.
fn list_span(text: &str, key: &str) -> Option<(usize, usize)> {
    let b = text.as_bytes();
    let skip_ws = |mut j: usize| {
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        j
    };
    let mut depth = 0usize;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => {
                let end = string_end(b, i)?;
                let j = skip_ws(end);
                if depth == 1 && b.get(j) == Some(&b':') && &text[i + 1..end - 1] == key {
                    let start = skip_ws(j + 1);
                    if b.get(start) != Some(&b'[') {
                        return None;
                    }
                    let (mut k, mut d) = (start, 0usize);
                    while k < b.len() {
                        match b[k] {
                            b'"' => {
                                k = string_end(b, k)?;
                                continue;
                            }
                            b'[' | b'{' => d += 1,
                            b']' | b'}' => {
                                d -= 1;
                                if d == 0 {
                                    return Some((start, k + 1));
                                }
                            }
                            _ => {}
                        }
                        k += 1;
                    }
                    return None;
                }
                i = end;
                continue;
            }
            b'{' | b'[' => depth += 1,
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const FILE: &str = r#"{
  "version": "7",
  "notes": { "owner": "web team", "order": [3, 1, 2] },
  "templates": {},
  "clients": [ { "name": "GA4", "template": "ga4_client", "data": { "allowedMeasurementIds": ["G-1"] } } ],
  "tags": [
    { "name": "GA4", "template": "ga4_tag", "comment": "keep me" },
    { "name": "Meta", "template": "meta_capi", "data": { "pixel_id": "1", "access_token": "EAAB", "nested": { "api_key": "k1" } } }
  ],
  "zzz": true
}
"#;

    fn setup(name: &str) -> (PathBuf, Arc<ContainerFile>) {
        let dir = std::env::temp_dir().join(format!("cf-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("container.json");
        std::fs::write(&path, FILE).unwrap();
        let f = ContainerFile::open(&path, Default::default(), Default::default()).unwrap();
        (path, f)
    }

    fn tag(v: Value) -> Tag {
        serde_json::from_value(v).unwrap()
    }

    fn rev(f: &ContainerFile) -> String {
        f.revision().unwrap()
    }

    #[test]
    fn a_save_keeps_every_other_key_as_written() {
        let (path, f) = setup("roundtrip");
        let before = f.resolved().generation;
        f.add(
            &rev(&f),
            tag(json!({ "name": "Log", "template": "ga4_tag",
                "fire_when": [{ "key": "event_name", "op": "equals", "value": "purchase" }] })),
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        // Everything before and after the tags list is byte for byte the same.
        let (start, end) = tags_span(FILE).unwrap();
        assert!(text.starts_with(&FILE[..start]), "{text}");
        assert!(text.ends_with(&FILE[end..]), "{text}");
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["tags"][0]["comment"], "keep me");
        assert_eq!(v["tags"][2]["fire_when"][0]["op"], "equals");
        assert!(v["tags"][2].get("enabled").is_none());
        assert_eq!(v["version"], "7");
        let after = f.resolved();
        assert!(after.generation > before);
        assert_eq!(after.container.tags.len(), 3);
        assert_eq!(f.revision().unwrap(), revision_of(text.as_bytes()));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn transformations_are_added_saved_and_checked() {
        let (path, f) = setup("transformations");
        let rule = json!({ "name": "Lower", "tags": ["Meta"], "steps": [{ "op": "format", "key": "event_name", "format": "lower" }] });
        f.set_transformations(&rev(&f), vec![rule.clone()]).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["transformations"][0]["name"], "Lower");
        assert_eq!(v["zzz"], true);
        assert!(
            text.starts_with(&FILE[..FILE.rfind("\n}").unwrap()]),
            "{text}"
        );
        let c = f.resolved().container;
        assert_eq!(c.transformations[0].tags, vec!["Meta"]);

        // A second save replaces the list where it now is.
        f.set_transformations(
            &rev(&f),
            vec![
                rule.clone(),
                json!({ "name": "Drop", "steps": [{ "op": "remove", "keys": ["x"] }] }),
            ],
        )
        .unwrap();
        assert_eq!(f.resolved().container.transformations.len(), 2);

        // A rule for an unknown tag doesn't load, so nothing is written.
        let before = std::fs::read_to_string(&path).unwrap();
        let bad = json!({ "name": "Bad", "tags": ["Nope"], "steps": [{ "op": "remove", "keys": ["x"] }] });
        assert!(
            matches!(f.set_transformations(&rev(&f), vec![bad]), Err(EditError::Invalid(m)) if m.contains("unknown tag"))
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

        // Deleting a tag a rule names is refused until the rule changes.
        assert!(matches!(
            f.delete(&rev(&f), "Meta"),
            Err(EditError::Invalid(_))
        ));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn an_invalid_change_writes_nothing() {
        let (path, f) = setup("invalid");
        let r = rev(&f);
        let bad = [
            json!({ "name": "X", "template": "no_such_template" }),
            json!({ "name": "X", "template": "ga4_tag",
                "fire_when": [{ "key": "a", "op": "matches_regex", "value": "(" }] }),
            json!({ "name": "  ", "template": "ga4_tag" }),
            json!({ "name": "GA4", "template": "ga4_tag" }),
            json!({ "name": "x".repeat(101), "template": "ga4_tag" }),
            json!({ "name": "X", "template": "ga4_tag", "data": { "api_key": "[redacted]" } }),
        ];
        for t in bad {
            let e = f.add(&r, tag(t.clone())).unwrap_err();
            assert!(matches!(e, EditError::Invalid(_)), "{t}: {e:?}");
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), FILE);
        assert_eq!(rev(&f), r);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn redacted_secrets_keep_the_stored_value() {
        let (path, f) = setup("secret");
        f.update(
            &rev(&f),
            "Meta",
            tag(json!({ "name": "Meta", "template": "meta_capi",
                "data": { "pixel_id": "2", "access_token": "[redacted]", "nested": { "api_key": "[redacted]" } } })),
        )
        .unwrap();
        let c = f.resolved().container;
        assert_eq!(c.tags[1].data["pixel_id"], "2");
        assert_eq!(c.tags[1].data["access_token"], "EAAB");
        assert_eq!(c.tags[1].data["nested"]["api_key"], "k1");
        // A secret the tag never had can't be kept.
        let e = f
            .update(
                &rev(&f),
                "Meta",
                tag(json!({ "name": "Meta", "template": "meta_capi",
                    "data": { "client_secret": "[redacted]" } })),
            )
            .unwrap_err();
        assert!(
            matches!(e, EditError::Invalid(ref m) if m.contains("data.client_secret")),
            "{e:?}"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn rename_turn_off_reorder_and_delete() {
        let (path, f) = setup("rename");
        f.update(
            &rev(&f),
            "Meta",
            tag(
                json!({ "name": "Meta CAPI", "template": "meta_capi", "enabled": false,
                "data": { "pixel_id": "1", "access_token": "[redacted]" } }),
            ),
        )
        .unwrap();
        let c = f.resolved().container;
        assert_eq!(c.tags[1].name, "Meta CAPI");
        assert!(!c.tags[1].enabled);
        assert_eq!(c.tags[1].data["access_token"], "EAAB");
        assert!(!c.tags[1].fires_for(&json!({ "event_name": "purchase" })));
        assert_eq!(
            f.update(
                &rev(&f),
                "Meta",
                tag(json!({ "name": "M", "template": "ga4_tag" }))
            ),
            Err(EditError::NotFound)
        );

        f.reorder(&rev(&f), &["Meta CAPI".into(), "GA4".into()])
            .unwrap();
        assert_eq!(f.resolved().container.tags[0].name, "Meta CAPI");
        assert!(matches!(
            f.reorder(&rev(&f), &["GA4".into()]),
            Err(EditError::Invalid(_))
        ));
        assert!(matches!(
            f.reorder(&rev(&f), &["GA4".into(), "GA4".into()]),
            Err(EditError::Invalid(_))
        ));

        f.delete(&rev(&f), "GA4").unwrap();
        assert_eq!(f.resolved().container.tags.len(), 1);
        assert_eq!(f.delete(&rev(&f), "GA4"), Err(EditError::NotFound));
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["tags"][0]["enabled"], false);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn a_stale_revision_is_refused() {
        let (path, f) = setup("conflict");
        let old = rev(&f);
        f.delete(&old, "GA4").unwrap();
        let e = f.delete(&old, "Meta").unwrap_err();
        assert_eq!(e, EditError::Conflict(rev(&f)));
        // A change made outside the server counts too.
        let now = rev(&f);
        let text = std::fs::read_to_string(&path)
            .unwrap()
            .replace("\"7\"", "\"8\"");
        std::fs::write(&path, text).unwrap();
        assert!(matches!(
            f.delete(&now, "Meta"),
            Err(EditError::Conflict(_))
        ));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn a_changed_file_is_reloaded_and_a_broken_one_ignored() {
        let (path, f) = setup("reload");
        let g = f.resolved().generation;
        // Make sure the modification time moves on.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(
            &path,
            FILE.replace("\"comment\": \"keep me\"", "\"enabled\": false"),
        )
        .unwrap();
        f.reload_if_changed();
        let r = f.resolved();
        assert!(r.generation > g);
        assert!(!r.container.tags[0].enabled);
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, "{ not json").unwrap();
        f.reload_if_changed();
        assert_eq!(f.resolved().generation, r.generation);
        assert_eq!(f.resolved().container.tags.len(), 2);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn the_builtin_container_is_read_only() {
        let f = ContainerFile::fixed(Container::builtin(), Default::default(), Default::default());
        assert!(f.revision().is_none());
        assert!(matches!(f.delete("x", "GA4"), Err(EditError::ReadOnly(_))));
    }

    #[test]
    fn finds_the_top_level_tags_only() {
        let text = r#"{"clients":[{"tags":[1]}],"x":"\"tags\": [","tags" : [ {"a":"]"} ] }"#;
        let (s, e) = tags_span(text).unwrap();
        assert_eq!(&text[s..e], r#"[ {"a":"]"} ]"#);
        assert!(tags_span(r#"{"x":{"tags":[]}}"#).is_none());
    }
}
