//! Explains an alert when it opens: the AI gets the alert's numbers, the
//! setup changes made in the hours before it, a few recent hits that show
//! it, and the container, and answers with the likely cause and what to
//! check first. One call per new alert, never per check.

use serde_json::{Value, json};

use super::{Ai, AiError, Ask, language, server_prompt};

const SYSTEM: &str = concat!(
    server_prompt!(),
    r#"Your task: explain an alert that just opened. An alert means the last hour of tracking looks wrong compared with the same hour on the days before: a key event dropped or stopped, a vendor is refusing sends, a tag is failing, a value platforms need went missing, hits went unclaimed, or an event name changed spelling. You get the alert (its kind, subject and numbers), the setup changes made in the 3 hours before it started, a few recent hits from this instance that show it (there may be none: hits are kept per instance), and the container.

Say what most likely happened, briefly. A setup change shortly before is a strong lead when it touches the same tag, event or vendor; say so and name it. Common causes outside the server: a site deploy that renamed an event or dropped a value, an expired or wrong access token or pixel id, a consent banner change, a measurement id change. Don't claim certainty the data doesn't give.

Answer with summary (one sentence: what is happening), likely_cause (one or two sentences) and check_first (the one thing to look at first, concrete)."#
);

fn schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["summary", "likely_cause", "check_first"],
        "properties": {
            "summary": { "type": "string" },
            "likely_cause": { "type": "string" },
            "check_first": { "type": "string" },
        },
    })
}

/// The alert's diagnosis, with the model that wrote it.
pub async fn diagnose(
    ai: &Ai,
    alert: &Value,
    hits: &[Value],
    container: &Value,
    lang: Option<&str>,
) -> Result<Value, AiError> {
    let input = json!({
        "language": language(lang),
        "alert": {
            "kind": alert["kind"],
            "site": alert["site"],
            "subject": alert["subject"],
            "severity": alert["severity"],
            "numbers": alert["numbers"],
            "text": alert["texts"]["en"],
        },
        "setup_changes_before": alert["related"],
        "recent_hits": hits,
        "container": container,
    });
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
