//! Explains one hit from the debugger: why each tag did or didn't fire, what
//! the vendors answered, and what to change.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{
    Ai, AiError, Ask, change_properties, language, parse_change, server_prompt, tag_names,
};

const SYSTEM: &str = concat!(
    server_prompt!(),
    r#"Your task: explain one hit. You get the hit's trace (the request, the events the client built, each tag's run and outcome, every send and the vendor's answer, the template logs, and in "triggers" each tag's fire_when conditions checked against each event with the actual value) and the container it ran on.

Say whether the hit went as it should. Look for: a tag that should have fired but didn't, and the condition that stopped it (a wrong event name, a case difference, a missing key); a tag that fired when it shouldn't; a failed or refused send and what the vendor's answer means (a wrong pixel id or token, a missing required parameter); a tag that threw or timed out; a hit no client claimed; missing values a platform needs, such as value and currency on a purchase or an event_id to deduplicate with the browser pixel.

verdict is "ok" when nothing needs changing, "warning" when the hit worked but something should be improved, "problem" when something was lost or broken. With "ok", findings can be empty. Put the most important finding first. Use the finding's tag field for the tag it is about, or an empty string."#
);

fn schema() -> Value {
    let mut finding = json!({
        "severity": { "type": "string", "enum": ["info", "warning", "problem"] },
        "title": { "type": "string" },
        "detail": { "type": "string" },
        "tag": { "type": "string" },
    });
    finding
        .as_object_mut()
        .expect("object")
        .extend(change_properties().as_object().expect("object").clone());
    let required: Vec<&String> = finding.as_object().expect("object").keys().collect();
    let required = json!(required);
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["verdict", "summary", "findings"],
        "properties": {
            "verdict": { "type": "string", "enum": ["ok", "warning", "problem"] },
            "summary": { "type": "string" },
            "findings": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": required,
                    "properties": finding,
                },
            },
        },
    })
}

/// A short digest of the container, so a cached answer is used only while
/// the container is the one it was given.
pub(super) fn digest(container: &Value) -> String {
    let bytes = serde_json::to_vec(container).unwrap_or_default();
    Sha256::digest(&bytes)[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Explains the hit `id`. `trace` comes from [`crate::traces::for_ai`] and
/// `container` is the console's view of the container it ran on.
pub async fn explain(
    ai: &Ai,
    id: u64,
    trace: &Value,
    container: &Value,
    lang: Option<&str>,
) -> Result<Value, AiError> {
    let lang = language(lang);
    let key = format!("explain:{id}:{lang}:{}", digest(container));
    if let Some(hit) = ai.cached(&key) {
        return Ok(hit);
    }
    let input = json!({ "language": lang, "trace": trace, "container": container });
    let answer = ai
        .ask(Ask {
            system: SYSTEM,
            schema: schema(),
            effort: "medium",
            input: &input,
        })
        .await?;
    let checked = check(&answer, container, id, ai.model());
    ai.remember(key, checked.clone());
    Ok(checked)
}

/// The answer as the console gets it: findings about tags the container
/// doesn't have dropped, and tag changes parsed, or dropped when they
/// don't load.
pub(super) fn check(answer: &Value, container: &Value, id: u64, model: &str) -> Value {
    let names = tag_names(container);
    let text = |v: &Value, k: &str| v[k].as_str().unwrap_or_default().to_string();
    let findings: Vec<Value> = answer["findings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|f| {
            let tag = f["tag"].as_str().unwrap_or_default();
            tag.is_empty() || names.contains(&tag)
        })
        .map(|f| {
            json!({
                "severity": text(f, "severity"),
                "title": text(f, "title"),
                "detail": text(f, "detail"),
                "tag": text(f, "tag"),
                "fix": text(f, "fix"),
                "change": parse_change(f["tag_json"].as_str().unwrap_or_default(), &names),
            })
        })
        .collect();
    let verdict = answer["verdict"]
        .as_str()
        .filter(|v| matches!(*v, "ok" | "warning" | "problem"))
        .unwrap_or("warning");
    json!({
        "trace": id,
        "verdict": verdict,
        "summary": text(answer, "summary"),
        "findings": findings,
        "model": model,
    })
}
