//! Reviews a container change before it is saved: the AI gets what the
//! change edits and what it did to recent hits when they were replayed, and
//! says whether that looks intended, pointing out effects that look like
//! mistakes, such as a platform no longer getting purchases.

use serde_json::{Value, json};

use super::{Ai, AiError, Ask, language, server_prompt};

const SYSTEM: &str = concat!(
    server_prompt!(),
    r#"Your task: review a container change before it is saved. You get the edits (tags added, deleted or changed and which fields, the tag order, transformations) and the result of replaying recent hits through the container before and after the change: groups of differences, each with a tag, a kind (stops: the tag no longer fires for that event; starts: it now does; param_added, param_removed, param_changed: a parameter of what the tag sends, with an example before and after; endpoint: where it sends; send_count: how many requests), how many hits it affects and an example. Pinned hits are test cases the user marked as important; a change to one deserves attention.

Judge whether the effects match what the edits look meant to do. Flag effects that are probably not intended: a platform that stops getting purchases or other conversions, a value or currency or an event id that disappears, user data sent where it wasn't, a tag that now fires for every event, an endpoint change. An effect the edit clearly aims at (a tag turned off stops firing) is fine.

Answer with verdict ("fine" when the effects look intended, "check" when something deserves a second look, "risky" when it likely breaks tracking), summary (one or two sentences) and concerns (short items, most important first; empty when fine)."#
);

fn schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["verdict", "summary", "concerns"],
        "properties": {
            "verdict": { "type": "string", "enum": ["fine", "check", "risky"] },
            "summary": { "type": "string" },
            "concerns": { "type": "array", "items": { "type": "string" } },
        },
    })
}

pub async fn review(
    ai: &Ai,
    edits: &Value,
    replay: &Value,
    lang: Option<&str>,
) -> Result<Value, AiError> {
    let input = json!({ "language": language(lang), "edits": edits, "replay": replay });
    let mut v = ai
        .ask(Ask {
            system: SYSTEM,
            schema: schema(),
            effort: "medium",
            input: &input,
        })
        .await?;
    if !matches!(v["verdict"].as_str(), Some("fine" | "check" | "risky")) {
        v["verdict"] = json!("check");
    }
    v["model"] = json!(ai.model());
    Ok(v)
}
