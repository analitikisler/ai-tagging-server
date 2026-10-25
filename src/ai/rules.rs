//! Writes a transformation from a request in plain words: the AI gets the
//! request, the container and the event keys seen in recent hits, and
//! answers with a rule. The rule is checked and previewed on the trace
//! buffer before the console shows it; one that doesn't load goes back to
//! the AI once with the error.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::{Ai, AiError, Ask, language, server_prompt};
use crate::traces::IP_KEYS;

const SYSTEM: &str = concat!(
    server_prompt!(),
    r#"Your task: write one transformation from the request you get. A transformation changes event data on the server before tags see it, like a Transformation in Google's sGTM. The rules run in order, each on the event as the rules before it left it; a tag's firing conditions are checked against the event as its rules changed it, and the client's own event is never changed.

A transformation is a JSON object:
{"name": "...", "enabled": true, "when": [conditions], "tags": ["tag name", ...], "steps": [steps]}
- name: short and clear, at most 100 characters, different from the existing transformations' names.
- when: firing conditions in the same form as a tag's fire_when ({"key", "op", "value"}); the rule runs only for events that match all of them. Leave it empty to run for every event.
- tags: the tags whose events it changes, by name, from the container's tags. Leave it empty for every tag. Only limit it when the request names platforms or tags, or the change would be wrong for the others (for example removing data GA4 needs).
- steps, run in order; keys are dotted for nested values, like user_data.address.city:
  {"op": "set", "key": "currency", "value": "EUR", "only_if_missing": true}   value is any JSON; only_if_missing keeps an existing value
  {"op": "copy", "from": "user_id", "to": "user_data.external_id"}
  {"op": "map", "key": "event_name", "to": "event_name", "table": {"Purchase": "purchase", "order_complete": "purchase"}, "default": null, "ignore_case": true}   looks the value up by its string form; no match keeps the value unless default is set; "to" may name another key, for example to add a margin from a product category
  {"op": "format", "key": "value", "format": "number"}   formats: lower, upper, trim, number (from "49,90" or "1,234.50"), string
  {"op": "remove", "keys": ["ip_override", "user_data.email_address"]}
  {"op": "keep", "keys": ["value", "currency", "transaction_id"]}   keeps only these keys and event_name

What a transformation can't do: change each element of an array such as items, do arithmetic, read other hits or call other servers. When the request needs one of these, or is unclear, leave rule_json empty and say in question what is possible instead, briefly.

Use the event keys you are given: they are the keys seen in recent hits on this server, with example values, so name them exactly as they are. event_names lists every event name seen and how often: when the event a request is about comes in several spellings (purchase and Purchase), make the rule's conditions match all of them, for example with matches_regex ^(?i)purchase$, since the rule runs before any name is fixed. A key the request needs but the hits don't have yet may still be right; say so in the explanation.

Answer with explanation (what the rule does and which events and tags it touches, in one to three sentences), rule_json (the transformation as a JSON object string, or empty) and question (empty when you wrote a rule). When you are given a previous attempt and the error it got, fix the rule."#
);

fn schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["explanation", "rule_json", "question"],
        "properties": {
            "explanation": { "type": "string" },
            "rule_json": { "type": "string" },
            "question": { "type": "string" },
        },
    })
}

/// Most keys and example values the model is shown.
const MAX_KEYS: usize = 300;
const EXAMPLES: usize = 3;
/// Most event names listed, each with how often it was seen.
const EVENT_NAMES: usize = 100;
const EXAMPLE_LEN: usize = 60;

/// The keys of `events`, dotted, with how often each appears and a few
/// example values. Arrays of objects list their elements' keys under
/// `key[]`. IP addresses never show as examples.
pub fn catalog(events: &[Value]) -> Value {
    #[derive(Default)]
    struct Seen {
        count: usize,
        examples: Vec<String>,
    }
    fn walk(v: &Value, at: &str, out: &mut BTreeMap<String, Seen>) {
        match v {
            Value::Object(m) => {
                for (k, x) in m {
                    let key = if at.is_empty() {
                        k.clone()
                    } else {
                        format!("{at}.{k}")
                    };
                    walk(x, &key, out);
                }
            }
            Value::Array(a) if a.iter().any(Value::is_object) => {
                out.entry(at.to_string()).or_default().count += 1;
                for x in a {
                    walk(x, &format!("{at}[]"), out);
                }
            }
            _ => {
                if out.len() >= MAX_KEYS && !out.contains_key(at) {
                    return;
                }
                let seen = out.entry(at.to_string()).or_default();
                seen.count += 1;
                let last = at.rsplit(['.', ']']).next().unwrap_or(at);
                if IP_KEYS.contains(&last) {
                    return;
                }
                let text = match v {
                    Value::String(s) => s.chars().take(EXAMPLE_LEN).collect(),
                    other => other.to_string().chars().take(EXAMPLE_LEN).collect(),
                };
                if seen.examples.len() < EXAMPLES && !seen.examples.contains(&text) {
                    seen.examples.push(text);
                }
            }
        }
    }
    let mut out = BTreeMap::new();
    for e in events {
        walk(e, "", &mut out);
    }
    let keys: serde_json::Map<String, Value> = out
        .into_iter()
        .map(|(k, s)| (k, json!({ "count": s.count, "examples": s.examples })))
        .collect();
    let mut names: BTreeMap<String, usize> = BTreeMap::new();
    for e in events {
        if let Some(n) = e["event_name"].as_str() {
            *names
                .entry(n.chars().take(EXAMPLE_LEN).collect())
                .or_default() += 1;
        }
    }
    let mut names: Vec<(String, usize)> = names.into_iter().collect();
    names.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    names.truncate(EVENT_NAMES);
    let names: serde_json::Map<String, Value> =
        names.into_iter().map(|(n, c)| (n, json!(c))).collect();
    json!({ "events": events.len(), "event_names": names, "keys": keys })
}

/// Writes a transformation for `request`. `check` loads a rule against the
/// container and previews it, returning the rule as it loaded and the
/// preview, or the reason it doesn't load.
pub async fn write(
    ai: &Ai,
    request: &str,
    container: &Value,
    catalog: &Value,
    lang: Option<&str>,
    check: impl Fn(Value) -> Result<(Value, Value), String>,
) -> Result<Value, AiError> {
    let lang = language(lang);
    let mut input = json!({
        "language": lang,
        "request": request,
        "container": {
            "tags": container["tags"],
            "transformations": container["transformations"],
        },
        "event_keys": catalog,
    });
    for attempt in 0..2 {
        let answer = ai
            .ask(Ask {
                system: SYSTEM,
                schema: schema(),
                effort: "medium",
                input: &input,
            })
            .await?;
        let text = |k: &str| answer[k].as_str().unwrap_or_default().trim().to_string();
        let rule_json = text("rule_json");
        if rule_json.is_empty() {
            return Ok(json!({
                "explanation": text("explanation"),
                "question": text("question"),
                "rule": null,
                "preview": null,
                "model": ai.model(),
            }));
        }
        let loaded = serde_json::from_str::<Value>(&rule_json)
            .map_err(|e| format!("not JSON: {e}"))
            .and_then(&check);
        match loaded {
            Ok((rule, preview)) => {
                return Ok(json!({
                    "explanation": text("explanation"),
                    "question": "",
                    "rule": rule,
                    "preview": preview,
                    "model": ai.model(),
                }));
            }
            Err(e) if attempt == 0 => {
                input["previous_attempt"] = json!(rule_json);
                input["error"] = json!(e);
            }
            Err(e) => {
                return Err(AiError::Failed(format!(
                    "the rule it wrote doesn't load: {e}"
                )));
            }
        }
    }
    unreachable!("the loop returns on its second attempt")
}
