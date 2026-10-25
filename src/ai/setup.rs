//! Sets a container up from a description in words: "a Shopify store, GA4,
//! Meta and TikTok purchases, Google Ads conversions for purchases". The AI
//! gets the description, the container as it is and the event keys seen in
//! recent hits, and answers with every tag and transformation the container
//! should have, and what to change on the website. The proposal must load as
//! a container before the console sees it; one that doesn't goes back to
//! the AI once with the error. Nothing is saved until it is applied, and
//! applying goes through the same check as any other save.
//!
//! Secrets are never asked for: the description is refused when it looks like
//! it holds a token, and the settings still to fill in are worked out here,
//! not by the model.

use std::sync::LazyLock;

use serde_json::{Value, json};

use super::{Ai, AiError, Ask, language, server_prompt};
use crate::integrations::{Kind, PROVIDERS};

/// The settings of each built-in template, for the prompt.
fn settings_list() -> String {
    let mut out = String::from(
        "- ga4_tag: no required settings (forwards GA4 hits as they came); stripUserData (true drops plain user data before forwarding).\n",
    );
    for p in PROVIDERS {
        let fields: Vec<String> = p
            .fields
            .iter()
            .map(|f| {
                let kind = match f.kind {
                    Kind::Secret => ", secret",
                    Kind::Toggle => ", true or false",
                    _ => "",
                };
                let req = if f.required { "required" } else { "optional" };
                format!("{} ({req}{kind})", f.key)
            })
            .collect();
        out.push_str(&format!(
            "- {} ({}): {}.\n",
            p.template,
            p.name,
            fields.join(", ")
        ));
    }
    out.push_str(
        "Every ad platform tag above also takes eventUserData (true lets it use the user_data the page sends with GA4 events, hashed, for matching). Set it to true whenever the website steps send user_data; without it the tag ignores that data.\n",
    );
    out
}

static SYSTEM: LazyLock<String> = LazyLock::new(|| {
    format!(
        "{}{}{}",
        server_prompt!(),
        r#"Your task: set the container up from a description. Answer with every tag and every transformation the container should have once it is set up, in order: keep the existing ones the description doesn't change, exactly as given ("[redacted]" values included), change or add what it asks for, and leave out what it asks to remove.

A tag is {"name", "template", "data", "fire_when", "enabled"}. Use the built-in templates unless the container has a custom one that fits. Their settings (tag data):
"#,
        settings_list()
            + r#"
The ad platform tags already skip events their platform doesn't use and events whose consent denies ad storage, so they need no fire_when for that; give fire_when only to limit a tag further. The GA4 client is set up by the server itself: don't add clients. For each conversion action in Google Ads, add one google_ads tag with its conversion_event. Fill in ids the description gives (pixel ids, account ids, conversion action ids, measurement ids). Leave every secret (tokens, client secrets, refresh tokens) and every id the description doesn't give as an empty string: the user fills them in afterwards. Never invent an id.

Add transformations only when the description or the event keys show a need (for example an event name a platform's mapping won't recognise, or a value sent as text).

web_steps: what to do on the website and in the web GTM container so the server gets what these tags need, as short concrete steps: the Google tag's server_container_url pointing at this server, the events and parameters to send (value, currency, transaction_id, event_id for deduplication with browser pixels, user_data for enhanced matching), and the platform specifics the description mentions (for example a Shopify store's customer events). Keep it to what this setup needs.

Answer with explanation (what the setup does, in two or three sentences), tags_json (a JSON array string), transformations_json (a JSON array string), web_steps, and question (empty unless the description is too unclear to set anything up; then explain what's missing and leave tags_json and transformations_json empty). When you are given a previous attempt and the error it got, fix it."#
    )
});

fn schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["explanation", "tags_json", "transformations_json", "web_steps", "question"],
        "properties": {
            "explanation": { "type": "string" },
            "tags_json": { "type": "string" },
            "transformations_json": { "type": "string" },
            "web_steps": { "type": "array", "items": { "type": "string" } },
            "question": { "type": "string" },
        },
    })
}

/// Whether a description seems to hold a token or key, which must not be
/// sent to the AI: a Meta token (EAA…), a Google refresh token (1//…), an
/// API key (sk-…), or any long run of letters and digits.
pub fn looks_secret(text: &str) -> bool {
    text.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '"' | '\'' | '(' | ')'))
        .any(|w| {
            let w = w.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '/');
            let long = w.len() >= 32
                && w.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./+=".contains(c))
                && w.chars().any(|c| c.is_ascii_digit())
                && w.chars().any(|c| c.is_ascii_alphabetic());
            long || (w.starts_with("EAA") && w.len() > 20)
                || w.starts_with("1//")
                || (w.starts_with("sk-") && w.len() > 20)
        })
}

/// The settings still to fill in: required settings of the built-in ad
/// platform tags that are empty. `[redacted]` is a stored secret, so it counts
/// as set.
pub fn needs(tags: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for t in tags {
        let template = t["template"].as_str().unwrap_or_default();
        let Some(p) = PROVIDERS.iter().find(|p| p.template == template) else {
            continue;
        };
        for f in p.fields.iter().filter(|f| f.required) {
            let empty = match &t["data"][f.key] {
                Value::Null => true,
                Value::String(s) => s.trim().is_empty(),
                _ => false,
            };
            if empty {
                out.push(json!({
                    "tag": t["name"],
                    "setting": f.key,
                    "label": f.label,
                    "secret": matches!(f.kind, Kind::Secret),
                }));
            }
        }
    }
    out
}

fn parse_list(text: &str, what: &str) -> Result<Value, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(json!([]));
    }
    let v: Value = serde_json::from_str(text).map_err(|e| format!("{what} isn't JSON: {e}"))?;
    if !v.is_array() {
        return Err(format!("{what} must be a JSON array"));
    }
    Ok(v)
}

/// Proposes a setup for `request`. `check` loads tags and transformations as
/// the container would and says why they don't load.
pub async fn propose(
    ai: &Ai,
    request: &str,
    container: &Value,
    catalog: &Value,
    lang: Option<&str>,
    check: impl Fn(&Value, &Value) -> Result<(), String>,
) -> Result<Value, AiError> {
    let mut input = json!({
        "language": language(lang),
        "description": request,
        "container": {
            "tags": container["tags"],
            "transformations": container["transformations"],
            "templates": container["templates"],
        },
        "event_keys": catalog,
    });
    for attempt in 0..2 {
        let answer = ai
            .ask(Ask {
                system: SYSTEM.as_str(),
                schema: schema(),
                effort: "high",
                input: &input,
            })
            .await?;
        let text = |k: &str| answer[k].as_str().unwrap_or_default().trim().to_string();
        let steps = answer["web_steps"].clone();
        if text("tags_json").is_empty() && !text("question").is_empty() {
            return Ok(json!({
                "explanation": text("explanation"), "question": text("question"),
                "proposal": null, "needs": [], "web_steps": steps, "model": ai.model(),
            }));
        }
        let loaded = parse_list(&text("tags_json"), "tags_json").and_then(|tags| {
            let rules = parse_list(&text("transformations_json"), "transformations_json")?;
            check(&tags, &rules)?;
            Ok((tags, rules))
        });
        match loaded {
            Ok((tags, rules)) => {
                let needs = needs(tags.as_array().map(Vec::as_slice).unwrap_or_default());
                return Ok(json!({
                    "explanation": text("explanation"), "question": "",
                    "proposal": { "tags": tags, "transformations": rules },
                    "needs": needs, "web_steps": steps, "model": ai.model(),
                }));
            }
            Err(e) if attempt == 0 => {
                input["previous_attempt"] = json!({
                    "tags_json": text("tags_json"),
                    "transformations_json": text("transformations_json"),
                });
                input["error"] = json!(e);
            }
            Err(e) => {
                return Err(AiError::Failed(format!(
                    "the setup it wrote doesn't load: {e}"
                )));
            }
        }
    }
    unreachable!("the loop returns on its second attempt")
}
