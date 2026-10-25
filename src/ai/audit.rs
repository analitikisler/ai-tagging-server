//! Audits the container against the hits in the trace buffer for sends that
//! cost without helping: tags that never fire, two tags sending the same
//! hit to one vendor, events a platform doesn't use, sends that always fail.
//!
//! The counting is done here, not by the model: the model gets these
//! numbers, not the raw hits, and picks and explains the findings. What each
//! finding would save is worked out here again from the numbers, so the
//! figures the console shows never come from the model.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde_json::{Value, json};

use super::{
    Ai, AiError, Ask, change_properties, language, parse_change, server_prompt, tag_names,
};

const SYSTEM: &str = concat!(
    server_prompt!(),
    r#"Your task: audit the container for cost. You get the container and counts the server made from the recent hits in its trace buffer: hits, events by name, and for each tag its runs, outcomes, the events it fired for, its sends (also split by event) and failures; groups of failing sends with the vendor's answer; hits where several tags sent to the same vendor host; and tag errors.

Find what makes the server send, retry or run more than it needs to, and what is broken. Look for:
- dead_tag: an enabled tag that never fired in these hits, or a disabled one still in the container.
- duplicate_send: two tags sending the same events to one vendor, such as two GA4 tags for one measurement id or two Meta tags for one pixel. Keep one.
- noisy_event: a tag firing for events its platform doesn't use. Meta, TikTok and Pinterest optimize on conversion and standard events (purchase, add_to_cart, begin_checkout, lead, sign_up, view_item, search, page_view); engagement events like scroll, user_engagement, timer, video progress or form_start cost sends and add nothing there. GA4 uses them, so don't flag them for GA4 unless the event is clearly junk.
- failing_send: sends a vendor keeps refusing (a wrong id or token, a bad parameter): each one is wasted and often retried.
- retry_waste: many retries behind the sends.
- other: anything else that wastes sends or work.

For each finding set change: "remove_tag" when the tag should go, "stop_for_events" when it should stop firing for the events you list in events (then give the tag with its new fire_when in tag_json), "fix_tag" when a setting must change (the changed tag in tag_json), "none" otherwise. Don't state savings as numbers: the server works them out from change, tag and events. Whenever the fix is a change to a tag, give the whole changed tag in tag_json, so it can be reviewed and saved in the tag editor; for a dead tag with a wrong condition, that is "fix_tag" with the corrected fire_when. Put trace ids from the counts in trace_ids as examples. Put the finding that saves the most first. The hits are only the last ones kept on one instance, so say so if a conclusion needs more traffic to be sure. If nothing is worth changing, return no findings and say so in the summary."#
);

fn schema() -> Value {
    let mut finding = json!({
        "kind": { "type": "string", "enum": ["dead_tag", "duplicate_send", "noisy_event", "failing_send", "retry_waste", "other"] },
        "severity": { "type": "string", "enum": ["info", "warning", "problem"] },
        "title": { "type": "string" },
        "detail": { "type": "string" },
        "tag": { "type": "string" },
        "events": { "type": "array", "items": { "type": "string" } },
        "change": { "type": "string", "enum": ["remove_tag", "stop_for_events", "fix_tag", "none"] },
        "trace_ids": { "type": "array", "items": { "type": "integer" } },
    });
    finding
        .as_object_mut()
        .expect("object")
        .extend(change_properties().as_object().expect("object").clone());
    let required = json!(
        finding
            .as_object()
            .expect("object")
            .keys()
            .collect::<Vec<_>>()
    );
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["summary", "findings"],
        "properties": {
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

/// Most examples kept per group, and most groups listed.
const SAMPLES: usize = 3;
const GROUPS: usize = 20;

#[derive(Default)]
struct TagCount {
    runs: u64,
    outcomes: BTreeMap<String, u64>,
    fired_for: BTreeMap<String, u64>,
    sends: u64,
    sends_failed: u64,
    /// Sends by the event that caused them. A hit's sends from one tag are
    /// shared out over the events the tag ran for in that hit.
    sends_by_event: BTreeMap<String, f64>,
    samples: Vec<u64>,
}

#[derive(Default)]
struct Group {
    count: u64,
    samples: Vec<u64>,
    detail: Option<String>,
}

impl Group {
    fn add(&mut self, id: u64) {
        self.count += 1;
        if self.samples.len() < SAMPLES {
            self.samples.push(id);
        }
    }
}

/// The counts the audit works from.
pub struct Counts {
    pub hits_ran: u64,
    tags: BTreeMap<String, TagCount>,
    /// Every trace id counted.
    ids: BTreeSet<u64>,
    events: BTreeSet<String>,
    pub json: Value,
}

fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split(['/', '?', '#']).next().unwrap_or(rest)
}

/// Counts `traces` (id and detail, as [`crate::traces::Traces::details`]
/// gives them) for the tags of `container`.
pub fn count(traces: &[(u64, Value)], container: &Value, retries: u64) -> Counts {
    let mut tags: BTreeMap<String, TagCount> = BTreeMap::new();
    for name in tag_names(container) {
        tags.entry(name.to_string()).or_default();
    }
    let mut events_by_name: BTreeMap<String, u64> = BTreeMap::new();
    let mut failing: BTreeMap<(String, String, String), Group> = BTreeMap::new();
    let mut duplicates: BTreeMap<(String, Vec<String>), Group> = BTreeMap::new();
    let mut errors: BTreeMap<(String, String), Group> = BTreeMap::new();
    let (mut hits, mut ran, mut sends_total) = (0u64, 0u64, 0u64);
    let mut ids = BTreeSet::new();
    let (mut first, mut last) = (u64::MAX, 0u64);

    for (id, t) in traces {
        hits += 1;
        ids.insert(*id);
        if let Some(at) = t["at"].as_u64() {
            first = first.min(at);
            last = last.max(at);
        }
        let r = &t["result"];
        if r.is_null() {
            continue;
        }
        ran += 1;
        let names: Vec<String> = r["events"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|e| e["event_name"].as_str().unwrap_or("(no name)").to_string())
            .collect();
        for n in &names {
            *events_by_name.entry(n.clone()).or_default() += 1;
        }
        // Each tag's runs in this hit, by event name.
        let mut runs_here: HashMap<String, BTreeMap<String, u64>> = HashMap::new();
        for run in r["tags"].as_array().into_iter().flatten() {
            let tag = run["tag"].as_str().unwrap_or_default().to_string();
            let event = run["event"]
                .as_u64()
                .and_then(|i| names.get(i as usize))
                .cloned()
                .unwrap_or_else(|| "(no name)".into());
            let error = run["error"].as_str();
            let outcome = match (run["outcome"].as_str().unwrap_or_default(), error) {
                ("failure", Some(_)) => "exception",
                (o, _) => o,
            };
            let c = tags.entry(tag.clone()).or_default();
            c.runs += 1;
            *c.outcomes.entry(outcome.to_string()).or_default() += 1;
            *c.fired_for.entry(event.clone()).or_default() += 1;
            if c.samples.len() < SAMPLES && !c.samples.contains(id) {
                c.samples.push(*id);
            }
            *runs_here
                .entry(tag.clone())
                .or_default()
                .entry(event)
                .or_default() += 1;
            if let Some(e) = error {
                let e: String = e.chars().take(200).collect();
                errors.entry((tag, e)).or_default().add(*id);
            }
        }
        let mut senders: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut sent_here: HashMap<String, u64> = HashMap::new();
        for s in r["sends"].as_array().into_iter().flatten() {
            sends_total += 1;
            let from = s["from"].as_str().unwrap_or_default().to_string();
            let host = host_of(s["url"].as_str().unwrap_or_default()).to_string();
            let outcome = match (&s["result"]["Ok"], &s["result"]["Err"]) {
                (Value::Number(n), _) => n.as_u64().map(|n| (n < 400, n.to_string())),
                (_, Value::String(e)) => Some((false, e.chars().take(120).collect())),
                _ => None,
            };
            let c = tags.entry(from.clone()).or_default();
            c.sends += 1;
            *sent_here.entry(from.clone()).or_default() += 1;
            if let Some((false, result)) = outcome {
                c.sends_failed += 1;
                let g = failing
                    .entry((from.clone(), host.clone(), result))
                    .or_default();
                g.add(*id);
                if g.detail.is_none() {
                    g.detail = s["detail"].as_str().map(|d| d.chars().take(300).collect());
                }
            }
            senders.entry(host).or_default().insert(from);
        }
        for (tag, sent) in sent_here {
            let Some(by_event) = runs_here.get(&tag) else {
                continue;
            };
            let total: u64 = by_event.values().sum();
            let c = tags.entry(tag).or_default();
            for (event, n) in by_event {
                *c.sends_by_event.entry(event.clone()).or_default() +=
                    sent as f64 * *n as f64 / total as f64;
            }
        }
        for (host, from) in senders {
            if from.len() > 1 {
                duplicates
                    .entry((host, from.into_iter().collect()))
                    .or_default()
                    .add(*id);
            }
        }
    }

    let per_1000 = |n: f64| {
        if ran == 0 {
            0.0
        } else {
            (n * 1000.0 / ran as f64 * 10.0).round() / 10.0
        }
    };
    let views: HashMap<&str, &Value> = container["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| Some((t["name"].as_str()?, t)))
        .collect();
    let tag_list: Vec<Value> = tags
        .iter()
        .map(|(name, c)| {
            let view = views.get(name.as_str());
            let by_event: BTreeMap<&String, f64> = c
                .sends_by_event
                .iter()
                .map(|(e, n)| (e, (n * 10.0).round() / 10.0))
                .collect();
            json!({
                "name": name,
                "template": view.map(|v| v["template"].clone()),
                "enabled": view.map(|v| v["enabled"].clone()),
                "in_container": view.is_some(),
                "runs": c.runs,
                "outcomes": c.outcomes,
                "fired_for": c.fired_for,
                "never_fired": c.runs == 0,
                "sends": c.sends,
                "sends_failed": c.sends_failed,
                "sends_per_1000_hits": per_1000(c.sends as f64),
                "sends_by_event": by_event,
                "sample_trace_ids": c.samples,
            })
        })
        .collect();
    let top = |mut v: Vec<Value>| {
        v.sort_by_key(|g| std::cmp::Reverse(g["count"].as_u64().unwrap_or(0)));
        v.truncate(GROUPS);
        v
    };
    let failing: Vec<Value> = failing
        .into_iter()
        .map(|((tag, host, result), g)| {
            json!({ "tag": tag, "host": host, "result": result, "vendor_answer": g.detail, "count": g.count, "sample_trace_ids": g.samples })
        })
        .collect();
    let duplicates: Vec<Value> = duplicates
        .into_iter()
        .map(|((host, from), g)| json!({ "host": host, "tags": from, "count": g.count, "sample_trace_ids": g.samples }))
        .collect();
    let errors: Vec<Value> = errors
        .into_iter()
        .map(|((tag, error), g)| json!({ "tag": tag, "error": error, "count": g.count, "sample_trace_ids": g.samples }))
        .collect();
    let json = json!({
        "hits": hits,
        "hits_ran": ran,
        "from_at": (first != u64::MAX).then_some(first),
        "to_at": (last != 0).then_some(last),
        "events_by_name": events_by_name,
        "sends": sends_total,
        "sends_per_1000_hits": per_1000(sends_total as f64),
        "tags": tag_list,
        "failing_sends": top(failing),
        "duplicate_sends": top(duplicates),
        "tag_errors": top(errors),
        "send_retries_since_start": retries,
    });
    Counts {
        hits_ran: ran,
        tags,
        ids,
        events: events_by_name.into_keys().collect(),
        json,
    }
}

impl Counts {
    /// The sends a change would stop, as (tag, event) pairs with their
    /// sends: every event of a removed tag, or the events listed.
    fn stopped(&self, change: &str, tag: &str, events: &[String]) -> Vec<((String, String), f64)> {
        let Some(c) = self.tags.get(tag) else {
            return Vec::new();
        };
        c.sends_by_event
            .iter()
            .filter(|(e, _)| match change {
                "remove_tag" => true,
                "stop_for_events" => events.contains(e),
                _ => false,
            })
            .map(|(e, n)| ((tag.to_string(), e.clone()), *n))
            .collect()
    }

    fn per_1000(&self, n: f64) -> f64 {
        if self.hits_ran == 0 {
            0.0
        } else {
            (n * 1000.0 / self.hits_ran as f64 * 10.0).round() / 10.0
        }
    }
}

/// Audits `container` against `traces`, its hits. `retries` is the
/// process's send retries so far.
pub async fn audit(
    ai: &Ai,
    traces: &[(u64, Value)],
    container: &Value,
    retries: u64,
    lang: Option<&str>,
) -> Result<Value, AiError> {
    let counts = count(traces, container, retries);
    if counts.hits_ran == 0 {
        return Err(AiError::NoHits);
    }
    let lang = language(lang);
    let newest = counts.ids.last().copied().unwrap_or(0);
    let key = format!(
        "audit:{newest}:{}:{lang}:{}",
        counts.hits_ran,
        super::explain::digest(container)
    );
    if let Some(hit) = ai.cached(&key) {
        return Ok(hit);
    }
    let input = json!({ "language": lang, "container": container, "counts": counts.json });
    let answer = ai
        .ask(Ask {
            system: SYSTEM,
            schema: schema(),
            effort: "high",
            input: &input,
        })
        .await?;
    let checked = check(&answer, container, &counts, ai.model());
    ai.remember(key, checked.clone());
    Ok(checked)
}

/// The answer as the console gets it: findings about unknown tags dropped,
/// events and trace ids kept only when the counts have them, tag changes
/// parsed, and what each finding saves worked out from the counts.
pub(super) fn check(answer: &Value, container: &Value, counts: &Counts, model: &str) -> Value {
    let names = tag_names(container);
    let text = |v: &Value, k: &str| v[k].as_str().unwrap_or_default().to_string();
    let mut stopped_all: HashMap<(String, String), f64> = HashMap::new();
    let mut findings = Vec::new();
    for f in answer["findings"].as_array().into_iter().flatten() {
        let tag = text(f, "tag");
        if !tag.is_empty() && !names.contains(&tag.as_str()) {
            continue;
        }
        let events: Vec<String> = f["events"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| e.as_str())
            .filter(|e| counts.events.contains(*e))
            .map(str::to_string)
            .collect();
        let trace_ids: Vec<u64> = f["trace_ids"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_u64)
            .filter(|id| counts.ids.contains(id))
            .take(10)
            .collect();
        let mut change = text(f, "change");
        if !matches!(
            change.as_str(),
            "remove_tag" | "stop_for_events" | "fix_tag"
        ) || tag.is_empty()
        {
            change = "none".into();
        }
        let stopped = counts.stopped(&change, &tag, &events);
        // Folded from 0.0: a float sum of nothing is -0.0.
        let saved = stopped.iter().fold(0.0, |a, (_, n)| a + n);
        for (k, n) in stopped {
            stopped_all.insert(k, n);
        }
        let edit = if change == "remove_tag" {
            Some(json!({ "action": "remove", "tag": { "name": tag } }))
        } else {
            parse_change(f["tag_json"].as_str().unwrap_or_default(), &names)
        };
        findings.push(json!({
            "kind": text(f, "kind"),
            "severity": text(f, "severity"),
            "title": text(f, "title"),
            "detail": text(f, "detail"),
            "tag": tag,
            "events": events,
            "trace_ids": trace_ids,
            "fix": text(f, "fix"),
            "sends_saved_per_1000_hits": counts.per_1000(saved),
            "change": edit,
        }));
    }
    let now = counts.json["sends_per_1000_hits"].as_f64().unwrap_or(0.0);
    let saved = counts.per_1000(stopped_all.values().fold(0.0, |a, n| a + n));
    json!({
        "summary": text(answer, "summary"),
        "findings": findings,
        "counts": {
            "hits": counts.json["hits"],
            "hits_ran": counts.hits_ran,
            "from_at": counts.json["from_at"],
            "to_at": counts.json["to_at"],
            "sends_per_1000_hits": now,
            "sends_per_1000_hits_after": ((now - saved).max(0.0) * 10.0).round() / 10.0,
        },
        "model": model,
    })
}
