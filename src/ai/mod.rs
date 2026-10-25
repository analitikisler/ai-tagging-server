//! The AI behind the admin console: it reads a hit or the recent hits and
//! the container, and answers with findings and suggested tag changes that a
//! person reviews and saves. Nothing here runs while a hit is handled.
//!
//! Every answer is JSON in a fixed schema, and is checked against the
//! container before the console sees it: a finding that names a tag the
//! container doesn't have, or a tag change that doesn't load, is dropped.

pub mod audit;
pub mod diagnose;
pub mod explain;
pub mod review;
pub mod rules;
pub mod setup;
#[cfg(test)]
mod tests;
pub mod todo;

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::container::{Tag, compile_conditions};

pub const DEFAULT_MODEL: &str = "claude-opus-5-5";
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const DEFAULT_DAILY_LIMIT: u32 = 200;
const API_VERSION: &str = "2026-10-21";
/// Lets the API answer a declined request with a fallback model it picks.
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
/// Models that take `fallbacks: "default"`.
const FALLBACK_MODELS: &[&str] = &[
    "claude-opus-5-5",
    "claude-opus-5",
    "claude-sonnet-5-5",
    "claude-fable-5-1",
];
const MAX_TOKENS: u32 = 16_000;
const TIMEOUT: Duration = Duration::from_secs(180);
/// Answers kept so the same question asked again costs nothing.
const CACHE_SIZE: usize = 100;

/// Dollars per million tokens: input, output, cache read. Cache writes cost
/// 1.25 times input.
const PRICES: &[(&str, f64, f64, f64)] = &[
    ("claude-opus-5-5", 4.0, 20.0, 0.20),
    ("claude-opus-5", 5.0, 25.0, 0.50),
    ("claude-sonnet-5-5", 2.0, 10.0, 0.20),
    ("claude-sonnet-5", 2.0, 10.0, 0.20),
    ("claude-haiku-5-5", 0.10, 0.50, 0.01),
    ("claude-fable-5-1", 10.0, 50.0, 0.25),
];

pub struct Ai {
    http: reqwest::Client,
    key: String,
    model: String,
    base_url: String,
    daily_limit: u32,
    usage: Mutex<Usage>,
    cache: Mutex<VecDeque<(String, Value)>>,
}

#[derive(Default)]
struct Usage {
    /// Days since the epoch, UTC, that `today` counts.
    day: u64,
    today: u32,
    calls: u64,
    failed: u64,
    refused: u64,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
}

/// One question: a fixed system prompt, the JSON schema the answer must
/// fit, how hard to think, and the input.
pub struct Ask<'a> {
    pub system: &'static str,
    pub schema: Value,
    pub effort: &'static str,
    pub input: &'a Value,
}

#[derive(Debug, PartialEq)]
pub enum AiError {
    /// The day's call limit is used up.
    Limit(u32),
    /// The API key was refused.
    Key,
    /// Rate limited or overloaded, still after a retry.
    Busy,
    /// The model declined to answer.
    Refused(String),
    /// The answer reached the output cap before it was finished.
    Truncated,
    /// Anything else: a network error, an unexpected answer.
    Failed(String),
    /// There is nothing to ask about yet, so nothing was asked.
    NoHits,
}

impl AiError {
    pub fn status(&self) -> u16 {
        match self {
            AiError::Limit(_) | AiError::Busy => 429,
            AiError::NoHits => 400,
            _ => 502,
        }
    }

    pub fn message(&self) -> String {
        match self {
            AiError::Limit(n) => format!(
                "the AI's daily limit of {n} calls on this instance is used up; it resets at midnight UTC (AI_DAILY_LIMIT)"
            ),
            AiError::Key => "the Anthropic API refused ANTHROPIC_API_KEY".into(),
            AiError::Busy => {
                "the Anthropic API is busy or rate limited; try again in a minute".into()
            }
            AiError::Refused(why) => format!("the model declined to answer ({why})"),
            AiError::Truncated => "the answer was cut off before it was finished; try again".into(),
            AiError::Failed(m) => format!("the AI call failed: {m}"),
            AiError::NoHits => {
                "no hits have run on this instance yet, so there is nothing to audit".into()
            }
        }
    }
}

fn today() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() / 86_400)
}

impl Ai {
    pub fn new(key: String, model: String, base_url: String, daily_limit: u32) -> Self {
        Ai {
            http: reqwest::Client::builder()
                .timeout(TIMEOUT)
                .build()
                .expect("http client"),
            key,
            model,
            base_url: base_url.trim_end_matches('/').to_string(),
            daily_limit: daily_limit.max(1),
            usage: Mutex::default(),
            cache: Mutex::default(),
        }
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// What the console shows: the model, the day's calls and the tokens and
    /// estimated cost since the process started.
    pub fn status(&self) -> Value {
        let u = self.usage.lock().expect("ai usage");
        let today = if u.day == today() { u.today } else { 0 };
        let cost = PRICES
            .iter()
            .find(|(m, ..)| *m == self.model)
            .map(|(_, i, o, r)| {
                let usd = (u.input_tokens as f64 * i
                    + u.cache_write_tokens as f64 * i * 1.25
                    + u.cache_read_tokens as f64 * r
                    + u.output_tokens as f64 * o)
                    / 1e6;
                (usd * 10_000.0).round() / 10_000.0
            });
        json!({
            "model": self.model,
            "daily_limit": self.daily_limit,
            "calls_today": today,
            "calls": u.calls,
            "failed": u.failed,
            "refused": u.refused,
            "input_tokens": u.input_tokens,
            "output_tokens": u.output_tokens,
            "cache_read_tokens": u.cache_read_tokens,
            "cache_write_tokens": u.cache_write_tokens,
            "est_cost_usd": cost,
        })
    }

    pub fn cached(&self, key: &str) -> Option<Value> {
        let cache = self.cache.lock().expect("ai cache");
        cache.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    pub fn remember(&self, key: String, answer: Value) {
        let mut cache = self.cache.lock().expect("ai cache");
        cache.retain(|(k, _)| *k != key);
        if cache.len() >= CACHE_SIZE {
            cache.pop_front();
        }
        cache.push_back((key, answer));
    }

    /// Counts a call against the day's limit, or refuses it.
    fn take_call(&self) -> Result<(), AiError> {
        let mut u = self.usage.lock().expect("ai usage");
        let day = today();
        if u.day != day {
            u.day = day;
            u.today = 0;
        }
        if u.today >= self.daily_limit {
            return Err(AiError::Limit(self.daily_limit));
        }
        u.today += 1;
        u.calls += 1;
        Ok(())
    }

    fn count(&self, f: impl FnOnce(&mut Usage)) {
        f(&mut self.usage.lock().expect("ai usage"));
    }

    /// Asks once and returns the answer's JSON. A rate limit, an overload or
    /// a dropped connection is retried once.
    pub async fn ask(&self, ask: Ask<'_>) -> Result<Value, AiError> {
        self.take_call()?;
        let result = self.ask_counted(ask).await;
        match &result {
            Err(AiError::Refused(_)) => self.count(|u| u.refused += 1),
            Err(_) => self.count(|u| u.failed += 1),
            Ok(_) => {}
        }
        result
    }

    async fn ask_counted(&self, ask: Ask<'_>) -> Result<Value, AiError> {
        let mut body = json!({
            "model": self.model,
            "max_tokens": MAX_TOKENS,
            "system": [{
                "type": "text",
                "text": ask.system,
                "cache_control": { "type": "ephemeral" },
            }],
            "messages": [{
                "role": "user",
                "content": serde_json::to_string(ask.input).unwrap_or_default(),
            }],
            "output_config": {
                "effort": ask.effort,
                "format": { "type": "json_schema", "schema": ask.schema },
            },
        });
        let fallback = FALLBACK_MODELS.contains(&self.model.as_str());
        if fallback {
            body["fallbacks"] = json!("default");
        }
        let body = serde_json::to_vec(&body).unwrap_or_default();
        let url = format!("{}/v1/messages", self.base_url);

        let mut attempt = 0;
        let (status, bytes) = loop {
            attempt += 1;
            let mut req = self
                .http
                .post(&url)
                .header("content-type", "application/json")
                .header("x-api-key", &self.key)
                .header("anthropic-version", API_VERSION)
                .body(body.clone());
            if fallback {
                req = req.header("anthropic-beta", FALLBACK_BETA);
            }
            let retry = match req.send().await {
                Ok(r) => {
                    let status = r.status().as_u16();
                    let wait = r
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok());
                    if matches!(status, 429 | 500 | 502 | 503 | 529) && attempt == 1 {
                        Some(wait.unwrap_or(2).min(10))
                    } else {
                        let bytes = r
                            .bytes()
                            .await
                            .map_err(|e| AiError::Failed(e.to_string()))?;
                        break (status, bytes);
                    }
                }
                Err(e) if attempt == 1 && !e.is_timeout() => Some(1),
                Err(e) => return Err(AiError::Failed(e.to_string())),
            };
            if let Some(secs) = retry {
                tokio::time::sleep(Duration::from_secs(secs)).await;
            }
        };
        let answer: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        match status {
            200 => {}
            401 | 403 => return Err(AiError::Key),
            429 | 529 => return Err(AiError::Busy),
            s => {
                let m = answer["error"]["message"].as_str().unwrap_or("no message");
                return Err(AiError::Failed(format!("HTTP {s}: {m}")));
            }
        }
        let n = |k: &str| answer["usage"][k].as_u64().unwrap_or(0);
        self.count(|u| {
            u.input_tokens += n("input_tokens");
            u.output_tokens += n("output_tokens");
            u.cache_read_tokens += n("cache_read_input_tokens");
            u.cache_write_tokens += n("cache_creation_input_tokens");
        });
        match answer["stop_reason"].as_str() {
            Some("refusal") => {
                let why = answer["stop_details"]["category"]
                    .as_str()
                    .unwrap_or("no reason given");
                return Err(AiError::Refused(why.to_string()));
            }
            Some("max_tokens") => return Err(AiError::Truncated),
            _ => {}
        }
        let text = answer["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect::<String>();
        serde_json::from_str(&text)
            .map_err(|e| AiError::Failed(format!("the answer wasn't the expected JSON: {e}")))
    }
}

/// The schema of a finding's suggested tag change, shared by both features.
/// `tag_json` is a whole tag as a JSON string, so the schema stays strict
/// while the tag's `data` can hold any settings.
fn change_properties() -> Value {
    json!({
        "fix": {
            "type": "string",
            "description": "What to change, in one or two sentences. Empty when there is nothing to change."
        },
        "tag_json": {
            "type": "string",
            "description": "The whole tag after the change, as a JSON object string in container format ({\"name\",\"template\",\"data\",\"fire_when\",\"enabled\"}), or an empty string when the fix isn't a tag change. Keep \"[redacted]\" values as they are."
        }
    })
}

/// A suggested tag change, parsed and checked to load: `edit` when it
/// replaces a tag the container has, `add` otherwise. None when there is
/// no change or it doesn't load.
fn parse_change(tag_json: &str, tags: &[&str]) -> Option<Value> {
    let tag_json = tag_json.trim();
    if tag_json.is_empty() {
        return None;
    }
    let mut tag: Tag = serde_json::from_str(tag_json).ok()?;
    if tag.name.trim().is_empty() || tag.template.trim().is_empty() || !tag.data.is_object() {
        return None;
    }
    compile_conditions(&mut tag.fire_when).ok()?;
    let action = if tags.contains(&tag.name.as_str()) {
        "edit"
    } else {
        "add"
    };
    Some(json!({ "action": action, "tag": tag }))
}

/// The tag names a container view lists.
fn tag_names(container: &Value) -> Vec<&str> {
    container["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["name"].as_str())
        .collect()
}

/// The answer language the console asked for.
fn language(lang: Option<&str>) -> &'static str {
    match lang {
        Some("tr") => "Turkish",
        _ => "English",
    }
}

/// What the model is told about this server, shared by every feature.
macro_rules! server_prompt {
    () => {
        r#"You are the assistant inside ai tagging server, a self-hosted server-side tagging server that works like Google's server-side Google Tag Manager (sGTM). You help the people who run it find and fix problems in their tracking setup.

How the server works:
- A hit arrives (for example a GA4 hit from gtag.js at /g/collect). Each client in the container may claim it; the claiming client turns it into one or more events (event data objects with keys like event_name, page_location, value, currency, items, user_data, x-ga-*).
- Then, for each event, every tag in the container is checked in order. A tag fires for an event when it is enabled and every condition in its fire_when matches the event. An empty fire_when fires for every event.
- A condition is {"key", "op", "value"}: key is an event data key, dotted for nested values; op is one of equals, not_equals, contains, starts_with, ends_with, matches_regex; value is a string. Comparison is on the string form of the event value. A missing key never matches equals, contains, starts_with, ends_with or matches_regex, and does match not_equals.
- A tag run ends as success, failure (the template called data.gtmOnFailure, or threw: then it has an error and shows as exception), or timeout.
- Tags send requests to vendors. Each send records the tag it came from, the URL, the HTTP status or a network error, and for refused sends the start of the vendor's answer.
- Built-in templates: ga4_client (claims GA4 hits), google_scripts_client (serves gtm.js and gtag.js), ga4_tag (forwards events to GA4), meta_capi (Meta Conversions API), tiktok_events (TikTok Events API), pinterest_capi (Pinterest Conversions API), google_ads (Google Ads conversions through Google's Data Manager API: on any event whose page URL has gclid, gbraid or wbraid it keeps the click in the first-party cookie _sst_ads; for its conversion_event, purchase by default, it sends the click and hashed user data with value, currency and transaction_id). Containers can add their own templates.
- Transformations (the container's "transformations" list) change event data before tags see it: they run in order, for the events their "when" conditions match and the tags they name (or every tag), with steps set, copy, map, format, remove and keep. A tag's fire_when is checked against the event as its transformations left it, and a trigger's transformed_by names the ones that ran. The trace's events are as the client built them, before any transformation.
- Plain user data (email, phone, name) is hashed before it reaches what you see and shows as "[hashed before sending]"; secret settings show as "[redacted]". Neither is a problem in itself.

Rules for your answer:
- Be concrete and brief. Name the tag, event, condition, key or vendor answer that shows the problem. Don't guess at things the data doesn't show; say what to check instead.
- Only name tags that are in the container you are given.
- When a tag change would fix a problem, always give the whole changed tag in tag_json, in container format, keeping every setting you don't change exactly as given, "[redacted]" values included. A fix described only in words can't be applied from the console.
- Write titles, details, fixes and the summary in the language you are asked for. Keep tag names, keys, event names and code as they are.

"#
    };
}
pub(crate) use server_prompt;
