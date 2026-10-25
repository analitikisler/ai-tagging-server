//! Turns a setup check into a to-do list: the AI reads the findings and
//! what the website and the container show, and answers with the steps to
//! take, most important first, split by where they are taken: on the
//! website (or its web GTM container) and on this server.

use serde_json::{Value, json};

use super::{Ai, AiError, Ask, language, server_prompt};

const SYSTEM: &str = concat!(
    server_prompt!(),
    r#"Your task: turn a setup check into a to-do list. The check lists findings, each with a kind, a status (bad, warn, info, ok), the side it is fixed on (website: the site or its web GTM container; server: this server's tags and settings; both) and its numbers. You also get what was read from the site and its web container (GA4 ids, the server URLs its Google tag sends to, browser pixels) and the container's tags.

Write the steps that make the setup complete, most important first: nothing works until hits arrive, then tags that can't send (empty settings, failures), then data the tags need (conversion events, value and currency, transaction_id, event_id for deduplication when a browser pixel also runs, user_data for matching, consent). Each step says exactly what to do and where (for the web container: which tag or setting in GTM; for this server: which tag and setting in the console). Leave out findings that are ok. Don't invent problems the findings don't show.

Each step is plain text for a person to follow: no JSON, no tag_json, no code beyond a setting or parameter name. Answer with summary (one or two sentences: how complete the setup is), website (steps for the site and its web container) and server (steps for this server)."#
);

fn schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["summary", "website", "server"],
        "properties": {
            "summary": { "type": "string" },
            "website": { "type": "array", "items": { "type": "string" } },
            "server": { "type": "array", "items": { "type": "string" } },
        },
    })
}

pub async fn todo(
    ai: &Ai,
    items: &Value,
    web: &Value,
    container: &Value,
    lang: Option<&str>,
) -> Result<Value, AiError> {
    let tags: Vec<Value> = container["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|t| json!({ "name": t["name"], "template": t["template"], "enabled": t["enabled"], "fire_when": t["fire_when"], "data": t["data"] }))
        .collect();
    let input = json!({ "language": language(lang), "findings": items, "web": web, "tags": tags });
    let mut v = ai
        .ask(Ask {
            system: SYSTEM,
            schema: schema(),
            effort: "medium",
            input: &input,
        })
        .await?;
    v["model"] = json!(ai.model());
    Ok(v)
}
