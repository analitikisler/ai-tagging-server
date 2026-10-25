//! The last hits this instance ran, for the admin page: which events arrived,
//! how each tag ended (and why it skipped) and what each platform answered.
//! Kept in memory only, so every instance has its own and a restart empties it.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

use crate::sandbox::{RunResult, TagRun};

/// Hits kept; the oldest is dropped past this.
pub const CAPACITY: usize = 200;

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub id: u64,
    /// Milliseconds since the Unix epoch.
    pub at: u64,
    /// Sent from the admin page's test button rather than by a visitor.
    pub test: bool,
    pub path: String,
    pub status: u16,
    pub ms: u64,
    pub client: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_error: Option<String>,
    pub events: Vec<EventSummary>,
    pub tags: Vec<TagRun>,
    pub sends: Vec<SendSummary>,
    pub logs: Vec<String>,
}

/// The parts of an event that help tell hits apart, without the visitor's
/// identifiers.
#[derive(Debug, Clone, Serialize)]
pub struct EventSummary {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    /// Whether the event carries an `event_id` the platforms deduplicate on.
    pub has_event_id: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SendSummary {
    pub from: String,
    pub method: String,
    /// Without the query, which can carry personal data.
    pub url: String,
    pub result: Result<u16, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<String>,
}

pub struct EventLog {
    entries: Mutex<VecDeque<Entry>>,
    next: AtomicU64,
}

impl Default for EventLog {
    fn default() -> Self {
        EventLog {
            entries: Mutex::new(VecDeque::with_capacity(CAPACITY)),
            next: AtomicU64::new(1),
        }
    }
}

impl EventLog {
    /// Keeps a hit that produced events or went wrong. Script downloads and
    /// unclaimed requests (bots, scanners) would only crowd the list.
    pub fn record(
        &self,
        path: &str,
        r: &RunResult,
        elapsed: Duration,
        test: bool,
    ) -> Option<Entry> {
        let worth = !r.events.is_empty() || r.client_error.is_some() || r.response.status >= 500;
        if !worth {
            return None;
        }
        let entry = Entry {
            id: self.next.fetch_add(1, Relaxed),
            at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            test,
            path: path.to_string(),
            status: r.response.status,
            ms: elapsed.as_millis() as u64,
            client: r.claimed_by.clone(),
            client_error: r.client_error.clone(),
            events: r.events.iter().map(summarize_event).collect(),
            tags: r.tags.clone(),
            sends: r
                .sends
                .iter()
                .map(|s| SendSummary {
                    from: s.from.clone(),
                    method: s.method.clone(),
                    url: s
                        .url
                        .split(['?', '#'])
                        .next()
                        .unwrap_or_default()
                        .to_string(),
                    result: s.result.clone().map_err(|e| without_queries(&e)),
                    response: s.detail.clone(),
                })
                .collect(),
            logs: r.logs.clone(),
        };
        let mut entries = self.entries.lock().expect("event log");
        if entries.len() >= CAPACITY {
            entries.pop_front();
        }
        entries.push_back(entry.clone());
        Some(entry)
    }

    /// Newest first, at most `limit`, optionally only hits where a tag whose
    /// name contains `tag` ran.
    pub fn recent(&self, limit: usize, tag: Option<&str>) -> Vec<Entry> {
        let entries = self.entries.lock().expect("event log");
        entries
            .iter()
            .rev()
            .filter(|e| tag.is_none_or(|t| e.tags.iter().any(|x| x.tag.contains(t))))
            .take(limit)
            .cloned()
            .collect()
    }
}

/// Drops the query from every URL in a message, such as a transport error
/// that names the URL it failed for.
pub fn without_queries(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_query = false;
    let mut in_url = false;
    for (i, c) in text.char_indices() {
        if c.is_whitespace() || matches!(c, ')' | '"' | '\'' | '>') {
            in_url = false;
            in_query = false;
        } else if !in_url && (text[i..].starts_with("https://") || text[i..].starts_with("http://"))
        {
            in_url = true;
        } else if in_url && c == '?' {
            in_query = true;
        }
        if !in_query {
            out.push(c);
        }
    }
    out
}

fn summarize_event(e: &Value) -> EventSummary {
    let text = |k: &str| e.get(k).and_then(Value::as_str).map(str::to_string);
    EventSummary {
        name: text("event_name").unwrap_or_default(),
        value: e.get("value").filter(|v| !v.is_null()).cloned(),
        currency: text("currency"),
        has_event_id: e
            .get("event_id")
            .is_some_and(|v| !v.is_null() && v.as_str() != Some("")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::{Response, SendRecord};
    use serde_json::json;

    fn hit(events: Vec<Value>, status: u16) -> RunResult {
        RunResult {
            claimed_by: Some("GA4".into()),
            response: Response {
                status,
                ..Default::default()
            },
            events,
            tags: vec![TagRun {
                tag: "Meta Conversions API".into(),
                event: 0,
                outcome: "skipped",
                error: None,
                skip: Some("consent:ad_storage=denied".into()),
            }],
            sends: vec![SendRecord {
                from: "GA4".into(),
                method: "POST".into(),
                url: "https://region1.google-analytics.com/g/collect?v=2&cid=123".into(),
                result: Ok(204),
                detail: None,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn keeps_hits_with_events_newest_first_and_drops_the_oldest() {
        let log = EventLog::default();
        assert!(
            log.record("/g/collect", &hit(vec![], 204), Duration::ZERO, false)
                .is_none()
        );
        for i in 0..CAPACITY + 5 {
            let e = json!({ "event_name": format!("e{i}"), "client_id": "123.456", "value": 5 });
            log.record("/g/collect", &hit(vec![e], 204), Duration::ZERO, false);
        }
        let recent = log.recent(1000, None);
        assert_eq!(recent.len(), CAPACITY);
        assert_eq!(recent[0].events[0].name, format!("e{}", CAPACITY + 4));
        assert_eq!(recent.last().unwrap().events[0].name, "e5");
        // No query (client id) and no visitor identifiers are kept.
        let text = serde_json::to_string(&recent[0]).unwrap();
        assert!(!text.contains("123"), "{text}");
        assert!(text.contains("consent:ad_storage=denied"));
        assert!(!recent[0].events[0].has_event_id);
    }

    #[test]
    fn transport_errors_lose_their_queries() {
        assert_eq!(
            without_queries(
                "error sending request for url (https://www.google-analytics.com/g/collect?v=2&_uip=1.2.3.4): tunnel error? no"
            ),
            "error sending request for url (https://www.google-analytics.com/g/collect): tunnel error? no"
        );
        assert_eq!(without_queries("timed_out"), "timed_out");
    }

    #[test]
    fn filters_by_tag_and_keeps_server_errors() {
        let log = EventLog::default();
        log.record(
            "/g/collect",
            &hit(vec![json!({"event_name":"page_view"})], 204),
            Duration::ZERO,
            false,
        );
        log.record("/x", &hit(vec![], 500), Duration::ZERO, true);
        assert_eq!(log.recent(10, None).len(), 2);
        assert_eq!(log.recent(10, Some("Meta")).len(), 2);
        assert_eq!(log.recent(10, Some("TikTok")).len(), 0);
        assert!(log.recent(10, None)[0].test);
    }
}
