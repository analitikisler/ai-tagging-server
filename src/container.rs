//! The container a server runs: its templates, clients and tags, and the
//! firing rules that decide which tags run for an event.

use std::collections::HashMap;
use std::path::Path;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::permissions::{self, Permissions};
use crate::transform::{self, Transformation};

const GA4_CLIENT: &str = include_str!("templates/ga4_client.js");
const GA4_TAG: &str = include_str!("templates/ga4_tag.js");
const GOOGLE_SCRIPTS_CLIENT: &str = include_str!("templates/google_scripts_client.js");
const PROVIDER_COMMON: &str = include_str!("templates/provider_common.js");
const META_CAPI: &str = include_str!("templates/meta_capi.js");
const TIKTOK_EVENTS: &str = include_str!("templates/tiktok_events.js");
const PINTEREST_CAPI: &str = include_str!("templates/pinterest_capi.js");
const GOOGLE_ADS: &str = include_str!("templates/google_ads.js");
const SNAPCHAT_CAPI: &str = include_str!("templates/snapchat_capi.js");
const REDDIT_CAPI: &str = include_str!("templates/reddit_capi.js");
const LINKEDIN_CAPI: &str = include_str!("templates/linkedin_capi.js");
const X_CAPI: &str = include_str!("templates/x_capi.js");
const BIGQUERY_EXPORT: &str = include_str!("templates/bigquery_export.js");
const SHOPIFY_CLIENT: &str = include_str!("templates/shopify_client.js");

#[derive(Debug, Clone)]
pub struct Container {
    pub version: String,
    /// Template name to source.
    pub templates: HashMap<String, String>,
    /// What each template loaded from a file may use. Built-in templates are
    /// trusted and have no entry, so they run unrestricted.
    pub permissions: HashMap<String, Permissions>,
    pub clients: Vec<Entry>,
    pub tags: Vec<Tag>,
    /// Changes to event data before tags see it, in order.
    pub transformations: Vec<Transformation>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Entry {
    pub name: String,
    pub template: String,
    #[serde(default = "empty_object")]
    pub data: Value,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Tag {
    pub name: String,
    pub template: String,
    #[serde(default = "empty_object")]
    pub data: Value,
    /// Every condition must match for the tag to fire. None means every event.
    #[serde(default)]
    pub fire_when: Vec<Condition>,
    /// A tag that is off never fires, and stays listed.
    #[serde(default = "yes")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Condition {
    /// Event data key, dotted for nested values (`x-ga-mp2-user_properties.plan`).
    pub key: String,
    pub op: Op,
    pub value: String,
    #[serde(skip)]
    regex: Option<Regex>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Equals,
    NotEquals,
    Contains,
    StartsWith,
    EndsWith,
    MatchesRegex,
}

#[derive(Debug, Deserialize)]
struct FileFormat {
    #[serde(default)]
    version: Option<String>,
    /// Template name to a file path, relative to the container file, or to a
    /// file and the permissions it holds.
    #[serde(default)]
    templates: HashMap<String, TemplateRef>,
    clients: Vec<Entry>,
    tags: Vec<Tag>,
    #[serde(default)]
    transformations: Vec<Transformation>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum TemplateRef {
    File(String),
    Spec {
        file: String,
        #[serde(default)]
        permissions: Value,
    },
}

fn empty_object() -> Value {
    Value::Object(Default::default())
}

fn yes() -> bool {
    true
}

fn provider(src: &str) -> String {
    format!("{PROVIDER_COMMON}\n{src}")
}

fn builtin_templates() -> HashMap<String, String> {
    HashMap::from([
        ("ga4_client".to_string(), GA4_CLIENT.to_string()),
        ("ga4_tag".to_string(), GA4_TAG.to_string()),
        (
            "google_scripts_client".to_string(),
            GOOGLE_SCRIPTS_CLIENT.to_string(),
        ),
        // Ad-platform tags share a prelude of helpers (hashing, enrichment).
        ("meta_capi".to_string(), provider(META_CAPI)),
        ("tiktok_events".to_string(), provider(TIKTOK_EVENTS)),
        ("pinterest_capi".to_string(), provider(PINTEREST_CAPI)),
        ("google_ads".to_string(), provider(GOOGLE_ADS)),
        ("snapchat_capi".to_string(), provider(SNAPCHAT_CAPI)),
        ("reddit_capi".to_string(), provider(REDDIT_CAPI)),
        ("linkedin_capi".to_string(), provider(LINKEDIN_CAPI)),
        ("x_capi".to_string(), provider(X_CAPI)),
        // Uses the same consent and event helpers.
        ("bigquery_export".to_string(), provider(BIGQUERY_EXPORT)),
        ("shopify_client".to_string(), SHOPIFY_CLIENT.to_string()),
    ])
}

/// What the built-in container is configured with.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Container and tag ids whose Google scripts are served.
    pub script_ids: Vec<String>,
    /// GA4 measurement ids hits are accepted for; `*` accepts any.
    pub measurement_ids: Vec<String>,
    /// Domains whose pages may send hits, with their subdomains. Empty means
    /// the request's own host.
    pub site_domains: Vec<String>,
    /// The GA4 client's `originCheck`: enforce, log or off.
    pub origin_check: String,
    /// The GA4 tag's `stripUserData`: drop plain user-provided data before
    /// forwarding to Google.
    pub strip_user_data: bool,
    /// The GA4 client's optional features.
    pub features: crate::features::Features,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            script_ids: vec![],
            measurement_ids: vec![],
            site_domains: vec![],
            origin_check: "log".into(),
            strip_user_data: false,
            features: Default::default(),
        }
    }
}

impl Container {
    /// The `templateDataStorage` map a template uses. The built-in ad-platform
    /// tags share one, so a visitor looked up for one platform is not looked
    /// up again for the others; every other template has its own.
    pub fn storage_scope(&self, template: &str) -> String {
        match self.templates.get(template) {
            Some(src) if src.starts_with(PROVIDER_COMMON) => "provider_common".into(),
            _ => template.into(),
        }
    }

    /// One GA4 client, one GA4 tag that fires for every event, and a client
    /// that serves Google's scripts for the ids in `script_ids` (none by
    /// default).
    #[cfg(test)]
    pub fn builtin() -> Self {
        Self::builtin_with_scripts(&[])
    }

    #[cfg(test)]
    pub fn builtin_with_scripts(script_ids: &[String]) -> Self {
        Self::builtin_with(&Settings {
            script_ids: script_ids.to_vec(),
            ..Default::default()
        })
    }

    pub fn builtin_with(s: &Settings) -> Self {
        Container {
            version: "builtin".into(),
            templates: builtin_templates(),
            permissions: HashMap::new(),
            clients: vec![
                Entry {
                    name: "GA4".into(),
                    template: "ga4_client".into(),
                    data: {
                        let mut data = serde_json::json!({
                            "allowedMeasurementIds": s.measurement_ids,
                            "siteDomains": s.site_domains,
                            "originCheck": s.origin_check,
                        });
                        if let Some(m) = data.as_object_mut() {
                            m.extend(s.features.client_data());
                        }
                        data
                    },
                },
                Entry {
                    name: "Google scripts".into(),
                    template: "google_scripts_client".into(),
                    data: serde_json::json!({ "allowedIds": s.script_ids }),
                },
            ],
            tags: vec![Tag {
                name: "GA4".into(),
                template: "ga4_tag".into(),
                data: if s.strip_user_data {
                    serde_json::json!({ "stripUserData": true })
                } else {
                    empty_object()
                },
                fire_when: vec![],
                enabled: true,
            }],
            transformations: vec![],
        }
    }

    #[cfg(test)]
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::from_text(&text, path)
    }

    /// Builds a container from a file's text, as if it were read from `path`:
    /// template paths are relative to it and every check of a load applies.
    pub fn from_text(text: &str, path: &Path) -> Result<Self, String> {
        let file: FileFormat =
            serde_json::from_str(text).map_err(|e| format!("{}: {e}", path.display()))?;
        let dir = path.parent().unwrap_or(Path::new("."));

        let mut templates = builtin_templates();
        let mut permissions = HashMap::new();
        for (name, spec) in &file.templates {
            let (rel, declared) = match spec {
                TemplateRef::File(f) => (f, Value::Null),
                TemplateRef::Spec { file, permissions } => (file, permissions.clone()),
            };
            let p = dir.join(rel);
            let text = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            let (src, held) = if rel.ends_with(".tpl") {
                let tpl = permissions::parse_tpl(&text).map_err(|e| format!("{name}: {e}"))?;
                (tpl.code, tpl.permissions)
            } else if declared.is_null() {
                (text, Permissions::new())
            } else {
                let held = permissions::from_json(&declared).map_err(|e| format!("{name}: {e}"))?;
                (text, held)
            };
            permissions::check_template(&src, Some(&held)).map_err(|e| format!("{name}: {e}"))?;
            templates.insert(name.clone(), src);
            permissions.insert(name.clone(), held);
        }

        let mut container = Container {
            version: file.version.unwrap_or_else(|| "file".into()),
            templates,
            permissions,
            clients: file.clients,
            tags: file.tags,
            transformations: file.transformations,
        };
        container.compile()?;
        for c in &container.clients {
            let ids = c
                .data
                .get("allowedMeasurementIds")
                .and_then(Value::as_array);
            if c.template == "ga4_client" && ids.is_none_or(|a| a.is_empty()) {
                tracing::warn!(
                    "client {}: no allowedMeasurementIds, so hits for any GA4 property are accepted",
                    c.name
                );
            }
        }
        Ok(container)
    }

    /// Checks template references and compiles regex conditions.
    pub fn compile(&mut self) -> Result<(), String> {
        let names = self
            .clients
            .iter()
            .map(|c| (&c.name, &c.template))
            .chain(self.tags.iter().map(|t| (&t.name, &t.template)));
        for (name, template) in names {
            if !self.templates.contains_key(template) {
                return Err(format!("{name}: unknown template {template}"));
            }
        }
        for c in self.clients.iter().filter(|c| c.template == "ga4_client") {
            crate::features::Features::from_client_data(&c.data)
                .map_err(|e| format!("{}: {e}", c.name))?;
        }
        for tag in &mut self.tags {
            compile_conditions(&mut tag.fire_when).map_err(|e| format!("{}: {e}", tag.name))?;
        }
        // Rules may also name the ad platform tags Integrations adds.
        let names: Vec<&str> = self
            .tags
            .iter()
            .map(|t| t.name.as_str())
            .chain(crate::integrations::PROVIDERS.iter().map(|p| p.name))
            .collect();
        transform::compile(&mut self.transformations, &names)?;
        Ok(())
    }
}

/// Compiles the regex of every `matches_regex` condition.
pub fn compile_conditions(conditions: &mut [Condition]) -> Result<(), String> {
    for c in conditions {
        if c.op == Op::MatchesRegex {
            c.regex = Some(Regex::new(&c.value).map_err(|e| e.to_string())?);
        }
    }
    Ok(())
}

/// Whether a tag fires for an event, and how each of its conditions went.
#[derive(Debug, Clone, Serialize)]
pub struct Trigger {
    pub event: usize,
    pub tag: String,
    pub fired: bool,
    /// False for a tag that is turned off: it never fires, whatever its
    /// conditions say.
    pub enabled: bool,
    pub conditions: Vec<Check>,
    /// The transformations that changed the event this tag saw.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub transformed_by: Vec<String>,
}

/// One `fire_when` condition against one event.
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub key: String,
    pub op: Op,
    pub value: String,
    /// The value compared, null when the event has no such key.
    pub actual: Option<String>,
    pub matched: bool,
}

impl Container {
    /// The firing decision for every event and tag, worked out the same way
    /// the engine decides which tags run.
    pub fn triggers(&self, events: &[Value]) -> Vec<Trigger> {
        let mut out = Vec::with_capacity(events.len() * self.tags.len());
        for (i, event) in events.iter().enumerate() {
            for tag in &self.tags {
                let (event, transformed_by) = self.event_for(tag, event);
                let conditions: Vec<Check> = tag
                    .fire_when
                    .iter()
                    .map(|c| {
                        let (actual, matched) = c.check(&event);
                        Check {
                            key: c.key.clone(),
                            op: c.op,
                            value: c.value.clone(),
                            actual,
                            matched,
                        }
                    })
                    .collect();
                out.push(Trigger {
                    event: i,
                    tag: tag.name.clone(),
                    fired: tag.enabled && conditions.iter().all(|c| c.matched),
                    enabled: tag.enabled,
                    conditions,
                    transformed_by,
                });
            }
        }
        out
    }
}

impl Container {
    /// The event `tag` sees after the transformations, and their names.
    pub fn event_for<'a>(
        &self,
        tag: &Tag,
        event: &'a Value,
    ) -> (std::borrow::Cow<'a, Value>, Vec<String>) {
        transform::view(&self.transformations, &tag.name, event)
    }
}

impl Tag {
    pub fn fires_for(&self, event: &Value) -> bool {
        self.enabled && self.fire_when.iter().all(|c| c.matches(event))
    }
}

impl Condition {
    pub fn matches(&self, event: &Value) -> bool {
        self.check(event).1
    }

    /// The value compared (None when the key is missing) and whether the
    /// condition holds.
    fn check(&self, event: &Value) -> (Option<String>, bool) {
        let mut v = event;
        for part in self.key.split('.') {
            match v.get(part) {
                Some(next) => v = next,
                None => return (None, self.op == Op::NotEquals),
            }
        }
        let actual = match v {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        };
        let matched = match self.op {
            Op::Equals => actual == self.value,
            Op::NotEquals => actual != self.value,
            Op::Contains => actual.contains(&self.value),
            Op::StartsWith => actual.starts_with(&self.value),
            Op::EndsWith => actual.ends_with(&self.value),
            Op::MatchesRegex => self.regex.as_ref().is_some_and(|r| r.is_match(&actual)),
        };
        (Some(actual), matched)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tag(conds: Value) -> Tag {
        let mut c = Container::builtin();
        c.tags = vec![
            serde_json::from_value(json!({
                "name": "T", "template": "ga4_tag", "fire_when": conds
            }))
            .unwrap(),
        ];
        c.compile().unwrap();
        c.tags.remove(0)
    }

    #[test]
    fn no_conditions_fires_always() {
        assert!(tag(json!([])).fires_for(&json!({"event_name": "x"})));
    }

    #[test]
    fn all_conditions_must_match() {
        let t = tag(json!([
            {"key": "event_name", "op": "equals", "value": "purchase"},
            {"key": "page_location", "op": "matches_regex", "value": "^https://example\\.com/"}
        ]));
        assert!(t.fires_for(
            &json!({"event_name": "purchase", "page_location": "https://example.com/x"})
        ));
        assert!(
            !t.fires_for(&json!({"event_name": "purchase", "page_location": "https://other.com/"}))
        );
        assert!(!t.fires_for(
            &json!({"event_name": "page_view", "page_location": "https://example.com/"})
        ));
    }

    #[test]
    fn nested_keys_and_numbers() {
        let t =
            tag(json!([{"key": "x-ga-mp2-user_properties.tier", "op": "equals", "value": "2"}]));
        assert!(t.fires_for(&json!({"x-ga-mp2-user_properties": {"tier": 2}})));
        assert!(!t.fires_for(&json!({})));
    }

    #[test]
    fn triggers_explain_each_condition() {
        let mut c = Container::builtin();
        c.tags.push(tag(json!([
            {"key": "event_name", "op": "equals", "value": "purchase"},
            {"key": "currency", "op": "not_equals", "value": "EUR"}
        ])));
        let events = [
            json!({"event_name": "purchase", "currency": "USD"}),
            json!({"event_name": "page_view"}),
        ];
        let t = c.triggers(&events);
        assert_eq!(t.len(), 4);
        assert!(t[0].fired && t[0].conditions.is_empty());
        assert!(t[1].fired);
        assert_eq!(t[1].conditions[1].actual.as_deref(), Some("USD"));
        let miss = &t[3];
        assert_eq!((miss.event, miss.tag.as_str(), miss.fired), (1, "T", false));
        assert!(!miss.conditions[0].matched);
        assert_eq!(miss.conditions[0].actual.as_deref(), Some("page_view"));
        assert!(miss.conditions[1].matched && miss.conditions[1].actual.is_none());
        for tr in &t {
            let tag = c.tags.iter().find(|x| x.name == tr.tag).unwrap();
            assert_eq!(tr.fired, tag.fires_for(&events[tr.event]));
        }
    }

    #[test]
    fn unknown_template_is_rejected() {
        let mut c = Container::builtin();
        c.tags[0].template = "missing".into();
        assert!(c.compile().is_err());
    }

    #[test]
    fn example_container_loads() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/container.json");
        let c = Container::from_file(&path).unwrap();
        assert_eq!(c.tags.len(), 2);
        assert!(c.templates.contains_key("purchase_logger"));
    }

    #[test]
    fn gallery_template_loads_with_its_declared_permissions() {
        let dir = std::env::temp_dir().join(format!("tpl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("t.tpl"),
            "___SANDBOXED_JS_FOR_SERVER___\nrequire('logToConsole')('x');\ndata.gtmOnSuccess();\n\n___SERVER_PERMISSIONS___\n[{\"instance\":{\"key\":{\"publicId\":\"logging\",\"versionId\":\"1\"},\"param\":[]},\"isRequired\":true}]\n",
        )
        .unwrap();
        let file = |perm_tpl: &str| {
            std::fs::write(
                dir.join("c.json"),
                format!(
                    r#"{{"templates":{{"t":"{perm_tpl}"}},"clients":[],"tags":[{{"name":"T","template":"t"}}]}}"#
                ),
            )
            .unwrap();
            Container::from_file(&dir.join("c.json"))
        };
        assert!(file("t.tpl").unwrap().permissions["t"].contains_key("logging"));
        std::fs::write(dir.join("u.js"), "require('logToConsole')('x');").unwrap();
        let e = file("u.js").unwrap_err();
        assert!(e.contains("`logging` permission"), "{e}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
